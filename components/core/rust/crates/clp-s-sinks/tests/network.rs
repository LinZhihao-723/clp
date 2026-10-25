//! The protocol-v2 network sink driven by sessions over C++ fixture archives: a partition connects
//! only once it has a result, sends the handshake, one frame per result numbered from 0 across
//! batches and a trailer, shuts down its side of the connection, and succeeds only once the peer
//! acknowledges every result, streaming exactly the records and the order of the collecting sink;
//! reruns stream again from 0, peers that close or reset the connection surface as
//! `SinkError::DownstreamClosed`, wrong acknowledgements fail the session, a dropped partition
//! sends no trailer, and partitions other than the destination's archive are rejected.

use std::io::Read;
use std::io::Write;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::num::NonZeroU32;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

use clp_s_reader::ArchiveId;
use clp_s_reader::ArchiveLocation;
use clp_s_reader::ExecutionConfig;
use clp_s_reader::MergeOrder;
use clp_s_reader::OutputRequirements;
use clp_s_reader::PartialSink;
use clp_s_reader::PartitionInfo;
use clp_s_reader::PartitionKind;
use clp_s_reader::PartitionSummary;
use clp_s_reader::Query;
use clp_s_reader::QueryOptions;
use clp_s_reader::ReadError;
use clp_s_reader::ReaderContext;
use clp_s_reader::RecordBatch;
use clp_s_reader::ResultOrder;
use clp_s_reader::ScanRequest;
use clp_s_reader::SearchSession;
use clp_s_reader::SessionReport;
use clp_s_reader::Sink;
use clp_s_reader::SinkControl;
use clp_s_reader::SinkError;
use clp_s_sinks::CollectSink;
use clp_s_sinks::NetworkDestination;
use clp_s_sinks::NetworkPartial;
use clp_s_sinks::NetworkSink;
use clp_s_wire::epoch_ns_to_wire_ms;

const SESSION_TOKEN: &str = "6f1c1c64-1e0a-4be8-a7a6-6a0e4d3c2b1a";
const ARCHIVE_ID: &str = "018e90e5-8b2a-4a61-a2fc-cac799936caf";
const TASK_INDEX: u64 = 42;

/// One decoded handshake: `(version, session_token, task_index, archive_id)`.
type Handshake = (u8, String, u64, String);

/// One decoded result frame: `(result_index, timestamp_ms, message)`.
type Frame = (u64, i64, String);

/// One decoded connection: the handshake, the result frames and the trailer's count, if any.
type Stream = (Handshake, Vec<Frame>, Option<u64>);

/// How a [`Peer`] answers one connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    /// Reads the connection to its end and acknowledges the results its trailer counts.
    Ack,

    /// Like [`Answer::Ack`], but waits before acknowledging.
    DelayedAck(Duration),

    /// Reads the connection to its end and acknowledges one result more than its trailer counts.
    WrongCount,

    /// Reads the connection to its end and answers with ten bytes that aren't an acknowledgement.
    WrongPrefix,

    /// Reads the connection to its end and closes it without acknowledging.
    Close,

    /// Reads one byte, then closes the connection with the rest unread, which resets it.
    Reset,
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
                if Answer::Reset == answer {
                    let mut byte = [0_u8; 1];
                    stream.read_exact(&mut byte)?;
                    received.push(byte[0]);
                    std::thread::sleep(Duration::from_millis(200));
                    connections.push(received);
                    continue;
                }
                stream.read_to_end(&mut received)?;
                let num_results = decode(&received)?.2.unwrap_or_default();
                match answer {
                    Answer::Ack => stream.write_all(&ack(num_results))?,
                    Answer::DelayedAck(delay) => {
                        std::thread::sleep(delay);
                        stream.write_all(&ack(num_results))?;
                    }
                    Answer::WrongCount => stream.write_all(&ack(num_results + 1))?,
                    Answer::WrongPrefix => {
                        let mut wrong = ack(num_results);
                        wrong[1] = 0xce;
                        stream.write_all(&wrong)?;
                    }
                    Answer::Close | Answer::Reset => {}
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

/// A sink that streams to a [`NetworkSink`] but fails each partition's second batch.
struct FailingNetworkSink {
    inner: NetworkSink,
}

impl Sink for FailingNetworkSink {
    type Output = u64;
    type Partial = FailingNetworkPartial;

    fn requirements(&self) -> OutputRequirements {
        self.inner.requirements()
    }

    fn merge_order(&self) -> MergeOrder {
        self.inner.merge_order()
    }

    fn open_partial(&self, partition: &PartitionInfo) -> Result<Self::Partial, SinkError> {
        Ok(FailingNetworkPartial {
            inner: self.inner.open_partial(partition)?,
            num_batches: 0,
        })
    }

    fn merge(&self, output: &mut Self::Output, partial: u64) -> Result<SinkControl, SinkError> {
        self.inner.merge(output, partial)
    }
}

/// The partial sink of [`FailingNetworkSink`].
struct FailingNetworkPartial {
    inner: NetworkPartial,
    num_batches: usize,
}

impl PartialSink for FailingNetworkPartial {
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

/// Decodes the bytes of one connection.
///
/// # Returns
///
/// The handshake, the result frames and the trailer's count, if the connection has a trailer, on
/// success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if bytes follow the trailer.
/// * Forwards [`serde::Deserialize::deserialize`]'s return values on failure (`data` is not a
///   handshake followed by frames and an optional trailer).
fn decode(data: &[u8]) -> anyhow::Result<Stream> {
    let mut remaining = data;
    let mut deserializer = rmp_serde::Deserializer::new(&mut remaining);
    let handshake: Handshake = serde::Deserialize::deserialize(&mut deserializer)?;
    let mut frames = Vec::new();
    while !remaining.is_empty() {
        let is_trailer = 0x91 == remaining[0];
        let mut deserializer = rmp_serde::Deserializer::new(&mut remaining);
        if is_trailer {
            let (num_results,): (u64,) = serde::Deserialize::deserialize(&mut deserializer)?;
            anyhow::ensure!(remaining.is_empty(), "bytes follow the trailer");
            return Ok((handshake, frames, Some(num_results)));
        }
        frames.push(serde::Deserialize::deserialize(&mut deserializer)?);
    }
    Ok((handshake, frames, None))
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
/// The bytes of `handshake`, `frames` and a trailer counting `frames`, encoded as C++ does, on
/// success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`rmp_serde::to_vec`]'s return values on failure.
fn encode(handshake: &Handshake, frames: &[Frame]) -> anyhow::Result<Vec<u8>> {
    let mut bytes = rmp_serde::to_vec(handshake)?;
    for frame in frames {
        bytes.extend(rmp_serde::to_vec(frame)?);
    }
    bytes.extend(rmp_serde::to_vec(&(frames.len() as u64,))?);
    Ok(bytes)
}

/// Runs `kql` over fixture `name` into a sink streaming to a peer that answers with `answer`.
///
/// # Returns
///
/// The session's error and the bytes the peer received, on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if the session succeeds, or the peer accepts other than one connection.
/// * Forwards [`Peer::join`]'s return values on failure.
fn run_failing_session(answer: Answer) -> anyhow::Result<(ReadError, Vec<u8>)> {
    let peer = Peer::new(vec![answer]);
    let Err(error) = run_session(
        "search.ts",
        "*",
        4096,
        Arc::new(NetworkSink::new(peer.destination())),
    ) else {
        anyhow::bail!("the session should fail when the peer answers with {answer:?}");
    };
    let mut connections = peer.join()?;
    anyhow::ensure!(1 == connections.len(), "{} connections", connections.len());
    Ok((error, connections.remove(0)))
}

/// # Returns
///
/// The frames the collecting sink's records of `kql` over fixture `name` must be streamed as, in
/// scan order, on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`run_session`]'s return values on failure.
fn expected_frames(name: &str, kql: &str) -> anyhow::Result<Vec<Frame>> {
    let records = run_session(name, kql, 4096, Arc::new(CollectSink::new()))?.output;
    Ok((0_u64..)
        .zip(records)
        .map(|(result_index, record)| {
            (
                result_index,
                record.timestamp_ns.map_or(0, epoch_ns_to_wire_ms),
                format!("{}\n", String::from_utf8_lossy(&record.json)),
            )
        })
        .collect())
}

#[test]
fn queries_without_results_never_connect() -> anyhow::Result<()> {
    let peer = Peer::new(Vec::new());
    let report = run_session(
        "logs.ts",
        "level: NO_SUCH_LEVEL",
        4096,
        Arc::new(NetworkSink::new(peer.destination())),
    )?;
    assert_eq!(report.output, 0);
    assert_eq!(peer.join()?, Vec::<Vec<u8>>::new());
    Ok(())
}

#[test]
fn results_follow_one_handshake_numbered_across_batches_and_end_with_a_trailer()
-> anyhow::Result<()> {
    for (name, kql, is_timestamped) in [
        ("logs.ts", "level: ERROR", true),
        ("logs.ts-tables4k", "*", true),
        ("search.default", "level: WARN", false),
        ("search.ts", "*", true),
    ] {
        let expected = expected_frames(name, kql)?;
        assert!(expected.len() > 3, "{name}: {}", expected.len());
        let peer = Peer::new(vec![Answer::Ack]);
        let report = run_session(name, kql, 3, Arc::new(NetworkSink::new(peer.destination())))?;
        let connections = peer.join()?;
        assert_eq!(connections.len(), 1, "{name}");
        let (handshake, frames, num_results) = decode(&connections[0])?;
        assert_eq!(
            handshake,
            (
                2,
                SESSION_TOKEN.to_owned(),
                TASK_INDEX,
                ARCHIVE_ID.to_owned()
            ),
            "{name}"
        );
        assert_eq!(report.output, frames.len() as u64, "{name}");
        assert_eq!(num_results, Some(report.output), "{name}");
        assert_eq!(frames, expected, "{name} ({kql})");
        assert_eq!(
            connections[0],
            encode(&handshake, &expected)?,
            "{name}: the bytes, up to the sink's EOF"
        );
        assert_eq!(
            frames.iter().any(|frame| 0 != frame.1),
            is_timestamped,
            "{name}"
        );
    }
    Ok(())
}

#[test]
fn reruns_stream_again_from_zero_over_a_new_connection() -> anyhow::Result<()> {
    let peer = Peer::new(vec![Answer::Ack, Answer::Ack]);
    let sink = Arc::new(NetworkSink::new(peer.destination()));
    run_session("search.ts", "*", 7, Arc::clone(&sink))?;
    run_session("search.ts", "*", 7, sink)?;
    let connections = peer.join()?;
    assert_eq!(connections.len(), 2);
    assert_eq!(connections[0], connections[1]);
    assert_eq!(
        decode(&connections[0])?.1,
        expected_frames("search.ts", "*")?
    );
    Ok(())
}

#[test]
fn delayed_acks_are_awaited() -> anyhow::Result<()> {
    const DELAY: Duration = Duration::from_millis(1500);

    let peer = Peer::new(vec![Answer::DelayedAck(DELAY)]);
    let started_at = Instant::now();
    let report = run_session(
        "search.ts",
        "*",
        4096,
        Arc::new(NetworkSink::new(peer.destination())),
    )?;
    let duration = started_at.elapsed();
    let connections = peer.join()?;
    assert!(duration >= DELAY, "the session took {duration:?}");
    assert_eq!(
        report.output,
        expected_frames("search.ts", "*")?.len() as u64
    );
    assert_eq!(connections.len(), 1);
    Ok(())
}

#[test]
fn peers_closing_without_an_ack_fail_the_session() -> anyhow::Result<()> {
    let (error, received) = run_failing_session(Answer::Close)?;
    assert!(
        matches!(&error, ReadError::Sink(sink) if matches!(**sink, SinkError::DownstreamClosed)),
        "{error:?}"
    );
    let expected = expected_frames("search.ts", "*")?;
    assert_eq!(
        decode(&received)?,
        (
            (
                2,
                SESSION_TOKEN.to_owned(),
                TASK_INDEX,
                ARCHIVE_ID.to_owned()
            ),
            expected.clone(),
            Some(expected.len() as u64)
        )
    );
    Ok(())
}

#[test]
fn peers_resetting_before_the_ack_fail_the_session() -> anyhow::Result<()> {
    let (error, _) = run_failing_session(Answer::Reset)?;
    assert!(
        matches!(&error, ReadError::Sink(sink) if matches!(**sink, SinkError::DownstreamClosed)),
        "{error:?}"
    );
    Ok(())
}

#[test]
fn wrong_acks_fail_the_session() -> anyhow::Result<()> {
    for answer in [Answer::WrongCount, Answer::WrongPrefix] {
        let (error, _) = run_failing_session(answer)?;
        assert!(
            matches!(&error, ReadError::Sink(sink) if matches!(**sink, SinkError::Custom(_))),
            "{answer:?}: {error:?}"
        );
    }
    Ok(())
}

#[test]
fn dropped_partitions_send_no_trailer() -> anyhow::Result<()> {
    let peer = Peer::new(vec![Answer::Close]);
    let error = run_session(
        "search.ts",
        "*",
        1,
        Arc::new(FailingNetworkSink {
            inner: NetworkSink::new(peer.destination()),
        }),
    )
    .expect_err("the second batch fails");
    assert!(
        matches!(&error, ReadError::Sink(sink) if matches!(**sink, SinkError::Custom(_))),
        "{error:?}"
    );
    let connections = peer.join()?;
    assert_eq!(connections.len(), 1);
    let (_, frames, num_results) = decode(&connections[0])?;
    assert_eq!(
        frames,
        expected_frames("search.ts", "*")?
            .into_iter()
            .take(1)
            .collect::<Vec<_>>()
    );
    assert_eq!(num_results, None);
    Ok(())
}

#[test]
fn closed_peers_fail_the_session() -> anyhow::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let peer = std::thread::spawn(move || listener.accept().map(drop));
    let error = run_session(
        "logs.ts-tables4k",
        "*",
        1,
        Arc::new(NetworkSink::new(NetworkDestination {
            host: address.ip().to_string(),
            port: address.port(),
            session_token: SESSION_TOKEN.to_owned(),
            task_index: TASK_INDEX,
            archive_id: ArchiveId::from(ARCHIVE_ID),
        })),
    )
    .expect_err("the peer closed the connection");
    peer.join()
        .map_err(|_| anyhow::anyhow!("the peer thread panicked"))??;
    assert!(
        matches!(&error, ReadError::Sink(sink) if matches!(**sink, SinkError::DownstreamClosed)),
        "{error:?}"
    );
    Ok(())
}

#[test]
fn unreachable_listeners_fail_the_session() -> anyhow::Result<()> {
    let address = TcpListener::bind("127.0.0.1:0")?.local_addr()?;
    let error = run_session(
        "search.ts",
        "*",
        4096,
        Arc::new(NetworkSink::new(NetworkDestination {
            host: address.ip().to_string(),
            port: address.port(),
            session_token: SESSION_TOKEN.to_owned(),
            task_index: TASK_INDEX,
            archive_id: ArchiveId::from(ARCHIVE_ID),
        })),
    )
    .expect_err("nothing listens on the port");
    assert!(
        matches!(&error, ReadError::Sink(sink) if matches!(**sink, SinkError::Io(_))),
        "{error:?}"
    );
    Ok(())
}

#[test]
fn other_archives_fail_the_session() -> anyhow::Result<()> {
    let peer = Peer::new(Vec::new());
    let error = run_session(
        "search.ts",
        "*",
        4096,
        Arc::new(NetworkSink::new(NetworkDestination {
            archive_id: ArchiveId::from("another-archive"),
            ..peer.destination()
        })),
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
fn top_k_partitions_are_rejected() {
    let peer = Peer::new(Vec::new());
    let error = NetworkSink::new(peer.destination())
        .open_partial(&PartitionInfo {
            index: 1,
            kind: PartitionKind::TopK,
            order: ResultOrder::Scan,
        })
        .expect_err("the top-k partition is not the destination's archive");
    assert!(matches!(error, SinkError::Custom(_)), "{error:?}");
}
