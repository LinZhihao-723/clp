//! The protocol-v3 network sink driven by sessions over C++ fixture archives: a partition connects
//! only once it has a result, sends the v3 handshake, an Arrow IPC stream with one schema, one
//! archive ID dictionary and record batches within the sink's limits, then a trailer, shuts down
//! its side of the connection, and succeeds only once the peer acknowledges every result,
//! streaming exactly the records and the order of the collecting sink; reruns stream the same bytes
//! again, peers that close without an acknowledgement or acknowledge wrongly fail the session, and
//! a dropped partition ends neither its IPC stream nor its connection with a trailer.

use std::io::Read;
use std::io::Write;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::num::NonZeroU32;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use arrow_array::Array;
use arrow_array::DictionaryArray;
use arrow_array::Int64Array;
use arrow_array::StringArray;
use arrow_array::types::Int32Type;
use arrow_ipc::MessageHeader;
use clp_s_arrow::ArrowNetworkPartial;
use clp_s_arrow::ArrowNetworkSink;
use clp_s_arrow::RESULT_SCHEMA;
use clp_s_arrow::ResultBatchLimits;
use clp_s_reader::ArchiveId;
use clp_s_reader::ArchiveLocation;
use clp_s_reader::ExecutionConfig;
use clp_s_reader::MergeOrder;
use clp_s_reader::OutputRequirements;
use clp_s_reader::PartialSink;
use clp_s_reader::PartitionInfo;
use clp_s_reader::PartitionSummary;
use clp_s_reader::Query;
use clp_s_reader::QueryOptions;
use clp_s_reader::ReadError;
use clp_s_reader::ReaderContext;
use clp_s_reader::RecordBatch;
use clp_s_reader::ScanRequest;
use clp_s_reader::SearchSession;
use clp_s_reader::SessionReport;
use clp_s_reader::Sink;
use clp_s_reader::SinkControl;
use clp_s_reader::SinkError;
use clp_s_sinks::CollectSink;
use clp_s_sinks::NetworkDestination;
use clp_s_wire::epoch_ns_to_wire_ms;

const SESSION_TOKEN: &str = "6f1c1c64-1e0a-4be8-a7a6-6a0e4d3c2b1a";
const ARCHIVE_ID: &str = "018e90e5-8b2a-4a61-a2fc-cac799936caf";
const TASK_INDEX: u64 = 42;

/// One decoded handshake: `(version, session_token, task_index, archive_id)`.
type Handshake = (u8, String, u64, String);

/// One decoded result: `(archive_id, timestamp_ms, message)`.
type Row = (String, i64, String);

/// One decoded connection.
#[derive(Debug, PartialEq, Eq)]
struct Connection {
    handshake: Handshake,

    /// The header type of every IPC message before the end-of-stream marker.
    message_types: Vec<MessageHeader>,

    /// The number of rows of each record batch.
    batch_num_rows: Vec<usize>,

    rows: Vec<Row>,

    /// The trailer's count, or `None` if the IPC stream doesn't end.
    num_results: Option<u64>,
}

/// How a [`Peer`] answers one connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    /// Reads the connection to its end and acknowledges the results its trailer counts.
    Ack,

    /// Reads the connection to its end and acknowledges one result more than its trailer counts.
    WrongCount,

    /// Reads the connection to its end and closes it without acknowledging.
    Close,
}

/// A listener capturing every byte of the connections it accepts, and answering each as told.
struct Peer {
    address: SocketAddr,
    connections: JoinHandle<anyhow::Result<(Vec<Vec<u8>>, TcpListener)>>,
}

impl Peer {
    /// Factory function.
    ///
    /// Binds a listener on a free local port that accepts one connection per answer of
    /// `answers`, one after the other, and answers each with its answer.
    ///
    /// # Returns
    ///
    /// The newly created peer.
    ///
    /// # Panics
    ///
    /// Panics if the listener can't be bound.
    fn new(answers: Vec<Answer>) -> Self {
        /// Bounds the wait for the sink's EOF, so that a sink that never shuts down its side of
        /// the connection fails the test instead of hanging it.
        const READ_TIMEOUT: Duration = Duration::from_secs(10);

        let listener = TcpListener::bind("127.0.0.1:0").expect("a local port can be bound");
        let address = listener.local_addr().expect("the listener has an address");
        let connections = std::thread::spawn(move || {
            let mut connections = Vec::with_capacity(answers.len());
            for answer in answers {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(READ_TIMEOUT))?;
                let mut received = Vec::new();
                stream.read_to_end(&mut received)?;
                let num_results = decode(&received)?.num_results.unwrap_or_default();
                match answer {
                    Answer::Ack => stream.write_all(&ack(num_results))?,
                    Answer::WrongCount => stream.write_all(&ack(num_results + 1))?,
                    Answer::Close => {}
                }
                connections.push(received);
            }
            Ok((connections, listener))
        });
        Self {
            address,
            connections,
        }
    }

    /// # Returns
    ///
    /// The destination of a task streaming to this peer.
    fn destination(&self) -> NetworkDestination {
        NetworkDestination {
            host: self.address.ip().to_string(),
            port: self.address.port(),
            session_token: SESSION_TOKEN.to_owned(),
            task_index: TASK_INDEX,
            archive_id: ArchiveId::from(ARCHIVE_ID),
        }
    }

    /// Waits for the expected connections, and checks that no other connection was made.
    ///
    /// # Returns
    ///
    /// The bytes received on each connection, in connection order, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if:
    ///   * The peer thread panicked.
    ///   * An extra connection was made.
    ///   * The peer failed to accept, read, decode or answer a connection.
    /// * Forwards [`TcpListener::set_nonblocking`]'s return values on failure.
    fn join(self) -> anyhow::Result<Vec<Vec<u8>>> {
        let (connections, listener) = self
            .connections
            .join()
            .map_err(|_| anyhow::anyhow!("the peer thread panicked"))??;
        listener.set_nonblocking(true)?;
        anyhow::ensure!(
            listener.accept().is_err(),
            "the sink made an unexpected connection"
        );
        Ok(connections)
    }
}

/// A sink that streams to an [`ArrowNetworkSink`] but fails each partition's second batch.
struct FailingArrowNetworkSink {
    inner: ArrowNetworkSink,
}

impl Sink for FailingArrowNetworkSink {
    type Output = u64;
    type Partial = FailingArrowNetworkPartial;

    fn requirements(&self) -> OutputRequirements {
        self.inner.requirements()
    }

    fn merge_order(&self) -> MergeOrder {
        self.inner.merge_order()
    }

    fn open_partial(&self, partition: &PartitionInfo) -> Result<Self::Partial, SinkError> {
        Ok(FailingArrowNetworkPartial {
            inner: self.inner.open_partial(partition)?,
            num_batches: 0,
        })
    }

    fn merge(&self, output: &mut Self::Output, partial: u64) -> Result<SinkControl, SinkError> {
        self.inner.merge(output, partial)
    }
}

/// The partial sink of [`FailingArrowNetworkSink`].
struct FailingArrowNetworkPartial {
    inner: ArrowNetworkPartial,
    num_batches: usize,
}

impl PartialSink for FailingArrowNetworkPartial {
    type Output = u64;

    fn consume(&mut self, batch: &RecordBatch<'_>) -> Result<SinkControl, SinkError> {
        self.num_batches += 1;
        if self.num_batches > 1 {
            return Err(SinkError::Custom("the second batch fails".into()));
        }
        self.inner.consume(batch)
    }

    fn finish(self, summary: &PartitionSummary) -> Result<Self::Output, SinkError> {
        self.inner.finish(summary)
    }
}

/// # Returns
///
/// The path of fixture archive `name`.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/archives")
        .join(format!("{name}.clps"))
}

/// Runs `kql` over fixture `name` into `sink`, with scan windows of `batch_num_rows` rows.
///
/// # Type Parameters
///
/// * `SinkType` - The sink the session pushes its records into.
///
/// # Returns
///
/// The session report on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`clp_s_reader::ReaderContextBuilder::build`]'s return values on failure.
/// * Forwards [`Query::from_kql`]'s return values on failure.
/// * Forwards [`clp_s_reader::ScanRequestBuilder::build`]'s return values on failure.
/// * Forwards [`clp_s_reader::SearchSessionBuilder::build`]'s return values on failure.
/// * Forwards [`SearchSession::run`]'s return values on failure.
fn run_session<SinkType: Sink>(
    name: &str,
    kql: &str,
    batch_num_rows: u32,
    sink: Arc<SinkType>,
) -> Result<SessionReport<SinkType::Output>, ReadError> {
    let mut config = ExecutionConfig::default();
    config.batch_num_rows = NonZeroU32::new(batch_num_rows).unwrap_or(NonZeroU32::MIN);
    let context = ReaderContext::builder().config(config).build()?;
    let request = ScanRequest::builder()
        .query(Query::from_kql(kql, &QueryOptions::default())?)
        .build()?;
    SearchSession::builder(context, Arc::new(request))
        .archive(
            ArchiveId::from(ARCHIVE_ID),
            ArchiveLocation::File(fixture(name)),
        )
        .build()?
        .run(sink)
}

/// # Returns
///
/// A sink streaming to `peer` with the default limits.
fn sink(peer: &Peer) -> Arc<ArrowNetworkSink> {
    Arc::new(ArrowNetworkSink::new(
        peer.destination(),
        ResultBatchLimits::default(),
    ))
}

/// Decodes the bytes of one connection.
///
/// # Returns
///
/// The decoded connection on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if:
///   * An IPC message isn't framed with the continuation marker.
///   * A column of a record batch has an unexpected type.
///   * Bytes follow the trailer.
/// * Forwards [`serde::Deserialize::deserialize`]'s return values on failure.
/// * Forwards [`arrow_ipc::root_as_message`]'s return values on failure.
/// * Forwards [`arrow_ipc::reader::StreamReader::try_new`]'s return values on failure.
/// * Forwards [`arrow_ipc::reader::StreamReader`]'s batch errors on failure.
fn decode(data: &[u8]) -> anyhow::Result<Connection> {
    let mut remaining = data;
    let mut deserializer = rmp_serde::Deserializer::new(&mut remaining);
    let handshake: Handshake = serde::Deserialize::deserialize(&mut deserializer)?;

    let mut message_types = Vec::new();
    let mut messages = remaining;
    let mut has_ended = false;
    while messages.len() >= 8 {
        anyhow::ensure!(messages[..4] == [0xff; 4], "missing continuation marker");
        let metadata_len = u32::from_le_bytes(messages[4..8].try_into()?) as usize;
        if 0 == metadata_len {
            has_ended = true;
            break;
        }
        if messages.len() < 8 + metadata_len {
            break;
        }
        let message = arrow_ipc::root_as_message(&messages[8..8 + metadata_len])
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        message_types.push(message.header_type());
        messages = &messages[(8 + metadata_len + usize::try_from(message.bodyLength())?)..];
    }

    let mut batch_num_rows = Vec::new();
    let mut rows = Vec::new();
    if !has_ended {
        return Ok(Connection {
            handshake,
            message_types,
            batch_num_rows,
            rows,
            num_results: None,
        });
    }
    let reader = arrow_ipc::reader::StreamReader::try_new(&mut remaining, None)?;
    anyhow::ensure!(reader.schema() == *RESULT_SCHEMA, "unexpected schema");
    for batch in reader {
        let batch = batch?;
        batch_num_rows.push(batch.num_rows());
        let archive_ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<DictionaryArray<Int32Type>>()
            .ok_or_else(|| anyhow::anyhow!("archive IDs aren't a dictionary"))?;
        let archive_id_values = archive_ids
            .values()
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| anyhow::anyhow!("archive ID values aren't strings"))?;
        let timestamps = batch
            .column(1)
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or_else(|| anyhow::anyhow!("timestamps aren't `Int64`"))?;
        let messages = batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| anyhow::anyhow!("messages aren't strings"))?;
        for row in 0..batch.num_rows() {
            rows.push((
                archive_id_values
                    .value(usize::try_from(archive_ids.keys().value(row))?)
                    .to_owned(),
                timestamps.value(row),
                messages.value(row).to_owned(),
            ));
        }
    }
    let mut deserializer = rmp_serde::Deserializer::new(&mut remaining);
    let (num_results,): (u64,) = serde::Deserialize::deserialize(&mut deserializer)?;
    anyhow::ensure!(remaining.is_empty(), "bytes follow the trailer");
    Ok(Connection {
        handshake,
        message_types,
        batch_num_rows,
        rows,
        num_results: Some(num_results),
    })
}

/// # Returns
///
/// The acknowledgement of `num_results` results: the msgpack array `[num_results]` with the count
/// encoded as a `uint 64`.
fn ack(num_results: u64) -> Vec<u8> {
    let mut bytes = vec![0x91, 0xcf];
    bytes.extend_from_slice(&num_results.to_be_bytes());
    bytes
}

/// # Returns
///
/// The rows the collecting sink's records of `kql` over fixture `name` must be streamed as, in
/// scan order, on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`run_session`]'s return values on failure.
fn expected_rows(name: &str, kql: &str) -> anyhow::Result<Vec<Row>> {
    let records = run_session(name, kql, 4096, Arc::new(CollectSink::new()))?.output;
    Ok(records
        .into_iter()
        .map(|record| {
            (
                ARCHIVE_ID.to_owned(),
                record.timestamp_ns.map_or(0, epoch_ns_to_wire_ms),
                String::from_utf8_lossy(&record.json).into_owned(),
            )
        })
        .collect())
}

/// # Returns
///
/// The handshake of a v3 stream to the test destination.
fn v3_handshake() -> Handshake {
    (
        3,
        SESSION_TOKEN.to_owned(),
        TASK_INDEX,
        ARCHIVE_ID.to_owned(),
    )
}

/// # Returns
///
/// The IPC message types of a stream with `num_batches` record batches.
fn message_types(num_batches: usize) -> Vec<MessageHeader> {
    let mut types = vec![MessageHeader::Schema, MessageHeader::DictionaryBatch];
    types.extend(std::iter::repeat_n(MessageHeader::RecordBatch, num_batches));
    types
}

#[test]
fn queries_without_results_never_connect() -> anyhow::Result<()> {
    let peer = Peer::new(Vec::new());
    let report = run_session("logs.ts", "level: NO_SUCH_LEVEL", 4096, sink(&peer))?;
    assert_eq!(report.output, 0);
    assert_eq!(peer.join()?, Vec::<Vec<u8>>::new());
    Ok(())
}

#[test]
fn results_stream_as_one_arrow_stream_followed_by_a_trailer() -> anyhow::Result<()> {
    for (name, kql, is_timestamped) in [
        ("logs.ts", "level: ERROR", true),
        ("logs.ts-tables4k", "*", true),
        ("search.default", "level: WARN", false),
        ("search.ts", "*", true),
    ] {
        let expected = expected_rows(name, kql)?;
        assert!(expected.len() > 3, "{name}: {}", expected.len());
        let peer = Peer::new(vec![Answer::Ack]);
        let report = run_session(name, kql, 3, sink(&peer))?;
        let connections = peer.join()?;
        assert_eq!(connections.len(), 1, "{name}");
        let connection = decode(&connections[0])?;

        assert_eq!(connection.handshake, v3_handshake(), "{name}");
        assert_eq!(
            connection.message_types,
            message_types(connection.batch_num_rows.len()),
            "{name}"
        );
        assert!(
            connection
                .batch_num_rows
                .iter()
                .all(|&num_rows| (1..=3).contains(&num_rows)),
            "{name}: one batch per scan window of 3 rows: {:?}",
            connection.batch_num_rows
        );
        assert_eq!(report.output, expected.len() as u64, "{name}");
        assert_eq!(connection.num_results, Some(report.output), "{name}");
        assert_eq!(connection.rows, expected, "{name} ({kql})");
        assert_eq!(
            connection.rows.iter().any(|row| 0 != row.1),
            is_timestamped,
            "{name}"
        );
    }
    Ok(())
}

#[test]
fn record_batches_are_split_at_the_limits() -> anyhow::Result<()> {
    let limits = ResultBatchLimits {
        max_num_rows: NonZeroUsize::new(2).expect("2 is nonzero"),
        ..ResultBatchLimits::default()
    };
    let peer = Peer::new(vec![Answer::Ack, Answer::Ack]);
    run_session("search.ts", "*", 4096, sink(&peer))?;
    run_session(
        "search.ts",
        "*",
        4096,
        Arc::new(ArrowNetworkSink::new(peer.destination(), limits)),
    )?;
    let connections = peer.join()?;
    let unlimited = decode(&connections[0])?;
    let limited = decode(&connections[1])?;

    assert!(
        unlimited
            .batch_num_rows
            .iter()
            .any(|&num_rows| num_rows > 2),
        "{:?}",
        unlimited.batch_num_rows
    );
    assert_eq!(
        limited.batch_num_rows,
        unlimited
            .batch_num_rows
            .iter()
            .flat_map(|&num_rows| {
                std::iter::repeat_n(2, num_rows / 2).chain((1 == num_rows % 2).then_some(1))
            })
            .collect::<Vec<_>>(),
        "each consumed batch splits into batches of 2 rows"
    );
    assert_eq!(limited.rows, unlimited.rows);
    assert_eq!(limited.rows, expected_rows("search.ts", "*")?);
    Ok(())
}

#[test]
fn reruns_stream_the_same_bytes_over_a_new_connection() -> anyhow::Result<()> {
    let peer = Peer::new(vec![Answer::Ack, Answer::Ack]);
    let sink = sink(&peer);
    run_session("search.ts", "*", 7, Arc::clone(&sink))?;
    run_session("search.ts", "*", 7, sink)?;
    let connections = peer.join()?;
    assert_eq!(connections.len(), 2);
    assert_eq!(connections[0], connections[1]);
    assert_eq!(
        decode(&connections[0])?.rows,
        expected_rows("search.ts", "*")?
    );
    Ok(())
}

#[test]
fn peers_closing_without_an_ack_fail_the_session() -> anyhow::Result<()> {
    let peer = Peer::new(vec![Answer::Close]);
    let error = run_session("search.ts", "*", 4096, sink(&peer))
        .expect_err("the session should fail without an acknowledgement");
    assert!(
        matches!(&error, ReadError::Sink(sink) if matches!(**sink, SinkError::DownstreamClosed)),
        "{error:?}"
    );
    let connection = decode(&peer.join()?.remove(0))?;
    let expected = expected_rows("search.ts", "*")?;
    assert_eq!(connection.num_results, Some(expected.len() as u64));
    assert_eq!(connection.rows, expected);
    Ok(())
}

#[test]
fn wrong_acks_fail_the_session() -> anyhow::Result<()> {
    let peer = Peer::new(vec![Answer::WrongCount]);
    let error = run_session("search.ts", "*", 4096, sink(&peer))
        .expect_err("the session should fail with a wrong acknowledgement");
    assert!(
        matches!(&error, ReadError::Sink(sink) if matches!(**sink, SinkError::Custom(_))),
        "{error:?}"
    );
    peer.join()?;
    Ok(())
}

#[test]
fn other_archives_fail_the_session() -> anyhow::Result<()> {
    let peer = Peer::new(Vec::new());
    let error = run_session(
        "search.ts",
        "*",
        4096,
        Arc::new(ArrowNetworkSink::new(
            NetworkDestination {
                archive_id: ArchiveId::from("another-archive"),
                ..peer.destination()
            },
            ResultBatchLimits::default(),
        )),
    )
    .expect_err("the session's archive is not the destination's");
    assert!(
        matches!(&error, ReadError::Sink(sink) if matches!(**sink, SinkError::Custom(_))),
        "{error:?}"
    );
    assert_eq!(peer.join()?, Vec::<Vec<u8>>::new());
    Ok(())
}

#[test]
fn dropped_partitions_end_neither_the_ipc_stream_nor_the_connection() -> anyhow::Result<()> {
    let peer = Peer::new(vec![Answer::Close]);
    let error = run_session(
        "search.ts",
        "*",
        1,
        Arc::new(FailingArrowNetworkSink {
            inner: ArrowNetworkSink::new(peer.destination(), ResultBatchLimits::default()),
        }),
    )
    .expect_err("the second batch fails");
    assert!(
        matches!(&error, ReadError::Sink(sink) if matches!(**sink, SinkError::Custom(_))),
        "{error:?}"
    );
    let connection = decode(&peer.join()?.remove(0))?;
    assert_eq!(connection.handshake, v3_handshake());
    assert_eq!(connection.message_types, message_types(1));
    assert_eq!(connection.num_results, None);
    Ok(())
}
