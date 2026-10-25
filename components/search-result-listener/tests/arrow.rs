//! Tests that drive a [`ResultListener`] with protocol-v3 connections, which stream their results
//! as an Arrow IPC stream: simulated search tasks over real TCP connections, and the clp-s reader's
//! Arrow network sink searching a fixture archive.

mod common;

use std::io::ErrorKind;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use clp_rust_utils::job_config::NetworkOutput;
use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::job_config::QueryJobStatus;
use clp_rust_utils::types::ArchiveId;
use clp_s_arrow::ArrowNetworkSink;
use clp_s_arrow::ResultBatchLimits;
use clp_s_arrow::ResultStreamWriter;
use clp_s_reader::ExecutionConfig;
use clp_s_reader::JsonRecordBuffer;
use clp_s_reader::MergeOrder;
use clp_s_reader::OutputRequirements;
use clp_s_reader::PartialSink;
use clp_s_reader::PartitionInfo;
use clp_s_reader::PartitionSummary;
use clp_s_reader::RecordBatch;
use clp_s_reader::Sink;
use clp_s_reader::SinkControl;
use clp_s_reader::SinkError;
use clp_s_sinks::CollectSink;
use clp_s_sinks::NetworkDestination;
use common::FakeJobStatusSource;
use futures::StreamExt;
use search_result_listener::ArrowResultBatch;
use search_result_listener::EncodedBatch;
use search_result_listener::ListenerConfig;
use search_result_listener::OutcomeFuture;
use search_result_listener::ResultEncoder;
use search_result_listener::ResultEncoding;
use search_result_listener::ResultListener;
use search_result_listener::ResultStream;
use search_result_listener::SearchResult;
use search_result_listener::SearchResultEncoding;
use search_result_listener::SessionConfig;
use search_result_listener::SessionOutcome;
use search_result_listener::SessionStats;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::sleep;
use tokio::time::timeout;

const ARCHIVE_ID: &str = "018e90e5-8b2a-4a61-a2fc-cac799936caf";
const OTHER_ARCHIVE_ID: &str = "4b0c2f8e-6d13-4f4e-9a57-1c2d3e4f5a6b";
const QUERY_JOB_ID: QueryJobId = 7;
const ARROW_PROTOCOL_VERSION: u64 = 3;

/// The longest a test waits for the listener before failing.
const TIMEOUT: Duration = Duration::from_secs(30);

const BASE_LISTENER_CONFIG: ListenerConfig = ListenerConfig {
    bind_addr: SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
    advertised_host: None,
    handshake_timeout: Duration::from_secs(10),
};

const BASE_SESSION_CONFIG: SessionConfig = SessionConfig {
    channel_size: NonZeroU32::new(64 * 1024 * 1024).expect("64 MiB is nonzero"),
    drain_grace_period: Duration::from_millis(200),
};

/// What a [`ForwardingEncoder`] did with claimed results.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Forwarded {
    /// A fully claimed record batch, forwarded as its IPC messages.
    Messages {
        dictionary: Bytes,
        record_batch: Bytes,
    },

    /// The claimed suffix of a partially duplicated record batch.
    Suffix {
        num_duplicates: usize,
        timestamps: Vec<i64>,
    },
}

/// The batch type of [`ForwardingEncoding`].
#[derive(Debug)]
struct Forwards(Vec<Forwarded>);

impl EncodedBatch for Forwards {
    fn size(&self) -> usize {
        self.0.len()
    }
}

/// A [`ResultEncoding`] that records how each record batch was claimed.
#[derive(Clone, Copy, Debug, Default)]
struct ForwardingEncoding;

impl ResultEncoding for ForwardingEncoding {
    type Batch = Forwards;
    type Encoder = ForwardingEncoder;

    fn encoder(&self, _archive_id: ArchiveId) -> ForwardingEncoder {
        ForwardingEncoder { batch: Vec::new() }
    }
}

/// The [`ResultEncoder`] of [`ForwardingEncoding`].
struct ForwardingEncoder {
    batch: Vec<Forwarded>,
}

impl ResultEncoder for ForwardingEncoder {
    type Batch = Forwards;

    fn encode(&mut self, _timestamp: i64, _message: &str) {
        panic!("protocol-v3 results should be encoded as record batches");
    }

    fn encode_arrow(&mut self, batch: &ArrowResultBatch, num_duplicates: usize) {
        if 0 == num_duplicates {
            let [dictionary, record_batch] = batch.ipc_messages();
            self.batch.push(Forwarded::Messages {
                dictionary: dictionary.clone(),
                record_batch: record_batch.clone(),
            });
            return;
        }
        self.batch.push(Forwarded::Suffix {
            num_duplicates,
            timestamps: batch.timestamps().values()[num_duplicates..].to_vec(),
        });
    }

    fn take_batch(&mut self) -> Forwards {
        Forwards(std::mem::take(&mut self.batch))
    }
}

/// A loopback listener with one running session whose job status the test controls.
///
/// # Type Parameters
///
/// * `EncodingType` - The encoding of the session's results.
struct Harness<EncodingType: ResultEncoding> {
    _listener: ResultListener<EncodingType>,
    network_output: NetworkOutput,
    status_source: FakeJobStatusSource,
    results: ResultStream<EncodingType::Batch>,
    outcome: OutcomeFuture,
}

impl<EncodingType: ResultEncoding> Harness<EncodingType> {
    /// Binds a listener and runs a session on it with `session_config` and `encoding`.
    async fn start(session_config: SessionConfig, encoding: EncodingType) -> Self {
        let listener = ResultListener::bind(BASE_LISTENER_CONFIG)
            .await
            .expect("binding a loopback listener should succeed");
        let session = listener.open_session(session_config, encoding);
        let network_output = session.network_output().clone();
        let status_source = FakeJobStatusSource::new();
        let (results, outcome) = session.run(QUERY_JOB_ID, status_source.clone());
        Self {
            _listener: listener,
            network_output,
            status_source,
            results,
            outcome,
        }
    }

    /// Connects a simulated attempt of task `task_index` and sends its v3 handshake for the
    /// archive [`ARCHIVE_ID`].
    async fn connect(&self, task_index: u64) -> FakeTask {
        self.connect_with_version(task_index, ARROW_PROTOCOL_VERSION)
            .await
    }

    /// Connects a simulated attempt of task `task_index` and sends its handshake of protocol
    /// `version` for the archive [`ARCHIVE_ID`].
    async fn connect_with_version(&self, task_index: u64, version: u64) -> FakeTask {
        let mut task = FakeTask {
            stream: TcpStream::connect((
                self.network_output.host.as_str(),
                self.network_output.port.get(),
            ))
            .await
            .expect("connecting to the listener should succeed"),
        };
        task.send(&encode_handshake(
            version,
            &self.network_output.session_token.to_string(),
            task_index,
        ))
        .await;
        task
    }

    /// # Returns
    ///
    /// The next batch the session emits.
    async fn next_batch(&mut self) -> EncodingType::Batch {
        timeout(TIMEOUT, self.results.next())
            .await
            .expect("a batch should arrive")
            .expect("the result stream shouldn't end yet")
    }

    /// Marks the job as terminated with `status`.
    ///
    /// # Returns
    ///
    /// The batches emitted after the ones already received, and the session's outcome.
    async fn finish(
        &mut self,
        status: QueryJobStatus,
    ) -> (Vec<EncodingType::Batch>, SessionOutcome) {
        self.status_source.set(Some(status));
        let batches = timeout(TIMEOUT, (&mut self.results).collect::<Vec<_>>())
            .await
            .expect("the result stream should end");
        let outcome = timeout(TIMEOUT, &mut self.outcome)
            .await
            .expect("the outcome should resolve")
            .expect("the session should succeed");
        (batches, outcome)
    }
}

/// A simulated attempt of a search task.
struct FakeTask {
    stream: TcpStream,
}

impl FakeTask {
    /// Sends `bytes` as they are.
    async fn send(&mut self, bytes: &[u8]) {
        self.stream
            .write_all(bytes)
            .await
            .expect("sending to the listener should succeed");
    }

    /// Shuts down the sending side of the connection, then waits until the listener closes it.
    ///
    /// # Returns
    ///
    /// The bytes the listener sent back.
    async fn shut_down_and_read_until_closed(mut self) -> Vec<u8> {
        self.shut_down().await;
        self.read_until_closed().await
    }

    /// Shuts down the sending side of the connection, as search tasks do after their trailer.
    async fn shut_down(&mut self) {
        self.stream
            .shutdown()
            .await
            .expect("sending EOF to the listener should succeed");
    }

    /// Waits until the listener closes the connection, either cleanly or with a reset.
    ///
    /// # Returns
    ///
    /// The bytes the listener sent before closing.
    async fn read_until_closed(mut self) -> Vec<u8> {
        let mut received = Vec::new();
        let mut chunk = [0_u8; 64];
        loop {
            match timeout(TIMEOUT, self.stream.read(&mut chunk))
                .await
                .expect("the listener should close the connection")
            {
                Ok(0) => return received,
                Ok(num_bytes_read) => received.extend_from_slice(&chunk[..num_bytes_read]),
                Err(e) => {
                    assert_eq!(
                        e.kind(),
                        ErrorKind::ConnectionReset,
                        "unexpected error: {e}"
                    );
                    return received;
                }
            }
        }
    }

    /// Checks that the listener keeps the connection open without sending anything.
    async fn expect_open(&mut self) {
        const OPEN_CHECK_DURATION: Duration = Duration::from_millis(200);

        let mut byte = [0_u8; 1];
        let read_result = timeout(OPEN_CHECK_DURATION, self.stream.read(&mut byte)).await;
        assert!(
            read_result.is_err(),
            "the listener shouldn't end the connection or send anything yet: {read_result:?}"
        );
    }
}

/// Encodes the Arrow IPC stream of one attempt of a task, one record batch per call.
struct Producer {
    writer: ResultStreamWriter,
    task_index: u64,
}

impl Producer {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A producer of the results of task `task_index`, whose dictionary holds `archive_id`.
    fn new(task_index: u64, archive_id: &str) -> Self {
        Self {
            writer: ResultStreamWriter::new(
                archive_id,
                ResultBatchLimits {
                    max_num_rows: NonZeroUsize::MAX,
                    max_size: usize::MAX,
                },
            ),
            task_index,
        }
    }

    /// # Returns
    ///
    /// The bytes of one record batch of the results at `result_indices`, preceded by the schema
    /// and the dictionary on the first call.
    fn batch(&mut self, result_indices: Range<u64>) -> Vec<u8> {
        let mut records = JsonRecordBuffer::new();
        let mut timestamps_ns = Vec::new();
        for result_index in result_indices {
            records
                .push_rendered(message(self.task_index, result_index).as_bytes())
                .expect("a small record should fit in the buffer");
            timestamps_ns.push(Some(timestamp(result_index) * 1_000_000));
        }
        let mut buffers = Vec::new();
        self.writer
            .write(&records, &timestamps_ns, &mut buffers)
            .expect("the batch should encode");
        buffers.iter().flat_map(|buffer| buffer.to_vec()).collect()
    }

    /// # Returns
    ///
    /// The end-of-stream marker followed by a trailer counting `num_results`.
    fn end(self, num_results: u64) -> Vec<u8> {
        let mut bytes: Vec<u8> = self
            .writer
            .finish()
            .expect("the stream should end")
            .iter()
            .flat_map(|buffer| buffer.to_vec())
            .collect();
        bytes.extend(encode_trailer(num_results));
        bytes
    }
}

/// A way a protocol-v3 connection violates the protocol after streaming its results.
#[derive(Clone, Copy, Debug)]
enum Violation {
    TrailerMismatch,
    DataAfterTrailer,
    ResultFrameAfterTheStream,
}

impl Violation {
    /// # Returns
    ///
    /// The bytes that end the stream of `producer`, which streamed 3 results, with the violation.
    fn end(self, producer: Producer) -> Vec<u8> {
        match self {
            Self::TrailerMismatch => producer.end(4),
            Self::DataAfterTrailer => {
                let mut bytes = producer.end(3);
                bytes.push(0x90);
                bytes
            }
            Self::ResultFrameAfterTheStream => {
                let mut bytes = producer.end(3);
                bytes.truncate(bytes.len() - encode_trailer(3).len());
                bytes.extend(encode_v2_results(0, 3..4));
                bytes.extend(encode_trailer(4));
                bytes
            }
        }
    }
}

/// A sink that streams to an [`ArrowNetworkSink`] but fails each partition's batch number
/// `failing_batch`, counted from 1.
struct FailingSink {
    inner: ArrowNetworkSink,
    failing_batch: usize,
}

impl Sink for FailingSink {
    type Output = u64;
    type Partial = FailingPartial;

    fn requirements(&self) -> OutputRequirements {
        self.inner.requirements()
    }

    fn merge_order(&self) -> MergeOrder {
        self.inner.merge_order()
    }

    fn open_partial(&self, partition: &PartitionInfo) -> Result<Self::Partial, SinkError> {
        Ok(FailingPartial {
            inner: self.inner.open_partial(partition)?,
            failing_batch: self.failing_batch,
            num_batches: 0,
        })
    }

    fn merge(&self, output: &mut Self::Output, partial: u64) -> Result<SinkControl, SinkError> {
        self.inner.merge(output, partial)
    }
}

/// The partial sink of [`FailingSink`].
struct FailingPartial {
    inner: clp_s_arrow::ArrowNetworkPartial,
    failing_batch: usize,
    num_batches: usize,
}

impl PartialSink for FailingPartial {
    type Output = u64;

    fn consume(&mut self, batch: &RecordBatch<'_>) -> Result<SinkControl, SinkError> {
        self.num_batches += 1;
        if self.num_batches == self.failing_batch {
            return Err(SinkError::Custom("the batch fails".into()));
        }
        self.inner.consume(batch)
    }

    fn finish(self, summary: &PartitionSummary) -> Result<Self::Output, SinkError> {
        self.inner.finish(summary)
    }
}

/// # Returns
///
/// The acknowledgement of `num_results` results.
fn ack(num_results: u64) -> Vec<u8> {
    let mut bytes = vec![0x91, 0xcf];
    bytes.extend_from_slice(&num_results.to_be_bytes());
    bytes
}

/// # Returns
///
/// The bytes of a handshake of `version` for task `task_index` of archive [`ARCHIVE_ID`].
fn encode_handshake(version: u64, session_token: &str, task_index: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    rmp::encode::write_array_len(&mut bytes, 4).expect("writing to a `Vec` shouldn't fail");
    rmp::encode::write_uint(&mut bytes, version).expect("writing to a `Vec` shouldn't fail");
    rmp::encode::write_str(&mut bytes, session_token).expect("writing to a `Vec` shouldn't fail");
    rmp::encode::write_uint(&mut bytes, task_index).expect("writing to a `Vec` shouldn't fail");
    rmp::encode::write_str(&mut bytes, ARCHIVE_ID).expect("writing to a `Vec` shouldn't fail");
    bytes
}

/// # Returns
///
/// The bytes of the protocol-v2 result frames at `result_indices` of task `task_index`.
fn encode_v2_results(task_index: u64, result_indices: Range<u64>) -> Vec<u8> {
    let mut bytes = Vec::new();
    for result_index in result_indices {
        rmp::encode::write_array_len(&mut bytes, 3).expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_uint(&mut bytes, result_index)
            .expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_sint(&mut bytes, timestamp(result_index))
            .expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_str(
            &mut bytes,
            &format!("{}\n", message(task_index, result_index)),
        )
        .expect("writing to a `Vec` shouldn't fail");
    }
    bytes
}

/// # Returns
///
/// The bytes of a trailer counting `num_results` results.
fn encode_trailer(num_results: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    rmp::encode::write_array_len(&mut bytes, 1).expect("writing to a `Vec` shouldn't fail");
    rmp::encode::write_uint(&mut bytes, num_results).expect("writing to a `Vec` shouldn't fail");
    bytes
}

/// # Returns
///
/// The framed IPC messages of `bytes`, which must hold only complete messages; the end-of-stream
/// marker counts as one.
fn split_messages(bytes: &[u8]) -> Vec<Bytes> {
    let mut messages = Vec::new();
    let mut remaining = bytes;
    while !remaining.is_empty() {
        let metadata_len = u32::from_le_bytes(
            remaining[4..8]
                .try_into()
                .expect("the metadata length should be 4 bytes"),
        ) as usize;
        let message_len = if 0 == metadata_len {
            8
        } else {
            let message = arrow_ipc::root_as_message(&remaining[8..8 + metadata_len])
                .expect("the metadata should be a message");
            8 + metadata_len
                + usize::try_from(message.bodyLength()).expect("the body length should be positive")
        };
        messages.push(Bytes::copy_from_slice(&remaining[..message_len]));
        remaining = &remaining[message_len..];
    }
    messages
}

/// # Returns
///
/// The timestamp of the result at `result_index`.
fn timestamp(result_index: u64) -> i64 {
    1_700_000_000_000 + i64::try_from(result_index).expect("test result indices fit in `i64`")
}

/// # Returns
///
/// The message of the result at `result_index` of task `task_index`.
fn message(task_index: u64, result_index: u64) -> String {
    format!("{{\"task\":{task_index},\"result\":{result_index}}}")
}

/// # Returns
///
/// The results at `result_indices` of task `task_index`, in index order.
fn expected_results(task_index: u64, result_indices: Range<u64>) -> Vec<SearchResult> {
    let archive_id = ARCHIVE_ID
        .parse::<ArchiveId>()
        .expect("test archive IDs are valid UUIDs");
    result_indices
        .map(|result_index| SearchResult {
            archive_id,
            timestamp: timestamp(result_index),
            message: message(task_index, result_index),
        })
        .collect()
}

/// # Returns
///
/// `results`, sorted by message.
fn sorted(mut results: Vec<SearchResult>) -> Vec<SearchResult> {
    results.sort_unstable_by(|left, right| left.message.cmp(&right.message));
    results
}

/// # Returns
///
/// The results of the search of `kql` over the fixture archive `name`, as a listener emits them.
///
/// # Panics
///
/// Panics if the search fails.
fn expected_fixture_results(name: &str, kql: &str) -> Vec<SearchResult> {
    let archive_id = ARCHIVE_ID
        .parse::<ArchiveId>()
        .expect("test archive IDs are valid UUIDs");
    run_fixture_search(name, kql, 4096, Arc::new(CollectSink::new()))
        .expect("the collecting search should succeed")
        .into_iter()
        .map(|record| SearchResult {
            archive_id,
            timestamp: record.timestamp_ns.map_or(0, |ns| ns.div_euclid(1_000_000)),
            message: String::from_utf8_lossy(&record.json).into_owned(),
        })
        .collect()
}

/// Searches the fixture archive `name` for `kql` into `sink`, with scan windows of
/// `batch_num_rows` rows.
///
/// # Type Parameters
///
/// * `SinkType` - The sink of the results.
///
/// # Returns
///
/// The sink's output on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`clp_s_reader::ReaderContextBuilder::build`]'s return values on failure.
/// * Forwards [`clp_s_reader::Query::from_kql`]'s return values on failure.
/// * Forwards [`clp_s_reader::ScanRequestBuilder::build`]'s return values on failure.
/// * Forwards [`clp_s_reader::SearchSessionBuilder::build`]'s return values on failure.
/// * Forwards [`clp_s_reader::SearchSession::run`]'s return values on failure.
fn run_fixture_search<SinkType: Sink>(
    name: &str,
    kql: &str,
    batch_num_rows: u32,
    sink: Arc<SinkType>,
) -> Result<SinkType::Output, clp_s_reader::ReadError> {
    let mut config = ExecutionConfig::default();
    config.batch_num_rows = NonZeroU32::new(batch_num_rows).unwrap_or(NonZeroU32::MIN);
    let context = clp_s_reader::ReaderContext::builder()
        .config(config)
        .build()?;
    let request = clp_s_reader::ScanRequest::builder()
        .query(clp_s_reader::Query::from_kql(
            kql,
            &clp_s_reader::QueryOptions::default(),
        )?)
        .build()?;
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../core/rust/tests/fixtures/archives")
        .join(format!("{name}.clps"));
    Ok(
        clp_s_reader::SearchSession::builder(context, Arc::new(request))
            .archive(
                clp_s_reader::ArchiveId::from(ARCHIVE_ID),
                clp_s_reader::ArchiveLocation::File(fixture),
            )
            .build()?
            .run(sink)?
            .output,
    )
}

/// # Returns
///
/// The destination of task `task_index` streaming to the session of `network_output`.
fn destination(network_output: &NetworkOutput, task_index: u64) -> NetworkDestination {
    NetworkDestination {
        host: network_output.host.to_string(),
        port: network_output.port.get(),
        session_token: network_output.session_token.to_string(),
        task_index,
        archive_id: clp_s_reader::ArchiveId::from(ARCHIVE_ID),
    }
}

/// # Returns
///
/// The record-batch limits of at most `max_num_rows` rows.
fn row_limits(max_num_rows: usize) -> ResultBatchLimits {
    ResultBatchLimits {
        max_num_rows: NonZeroUsize::new(max_num_rows).expect("the row limit should be nonzero"),
        ..ResultBatchLimits::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn v3_attempt_emits_every_result_in_order_and_is_acknowledged() {
    let mut harness = Harness::start(BASE_SESSION_CONFIG, SearchResultEncoding).await;
    let mut task = harness.connect(0).await;
    let mut producer = Producer::new(0, ARCHIVE_ID);
    for result_indices in [0..3, 3..1000, 1000..1001] {
        task.send(&producer.batch(result_indices)).await;
    }
    task.send(&producer.end(1001)).await;
    let acknowledgement = task.shut_down_and_read_until_closed().await;
    let (batches, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(acknowledgement, ack(1001));
    assert_eq!(batches.concat(), expected_results(0, 0..1001));
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: 1001,
            ..SessionStats::default()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fully_claimed_batches_are_forwarded_byte_exactly() {
    let mut harness = Harness::start(BASE_SESSION_CONFIG, ForwardingEncoding).await;
    let mut task = harness.connect(0).await;
    let mut producer = Producer::new(0, ARCHIVE_ID);
    let first = producer.batch(0..5);
    let second = producer.batch(5..7);
    task.send(&first).await;
    task.send(&second).await;
    task.send(&producer.end(7)).await;
    let acknowledgement = task.shut_down_and_read_until_closed().await;
    let (batches, _) = harness.finish(QueryJobStatus::Succeeded).await;

    let first_messages = split_messages(&first);
    let second_messages = split_messages(&second);
    assert_eq!(first_messages.len(), 3, "schema, dictionary, record batch");
    assert_eq!(second_messages.len(), 1, "record batch");
    assert_eq!(acknowledgement, ack(7));
    assert_eq!(
        batches
            .into_iter()
            .flat_map(|batch| batch.0)
            .collect::<Vec<_>>(),
        [
            Forwarded::Messages {
                dictionary: first_messages[1].clone(),
                record_batch: first_messages[2].clone(),
            },
            Forwarded::Messages {
                dictionary: first_messages[1].clone(),
                record_batch: second_messages[0].clone(),
            },
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partially_duplicated_batch_forwards_only_its_unclaimed_suffix() {
    let mut harness = Harness::start(BASE_SESSION_CONFIG, ForwardingEncoding).await;
    let mut first_attempt = harness.connect(2).await;
    let mut first_producer = Producer::new(2, ARCHIVE_ID);
    first_attempt.send(&first_producer.batch(0..4)).await;
    let first_batch = harness.next_batch().await;
    drop(first_attempt);

    let mut second_attempt = harness.connect(2).await;
    let mut second_producer = Producer::new(2, ARCHIVE_ID);
    second_attempt.send(&second_producer.batch(0..2)).await;
    second_attempt.send(&second_producer.batch(2..6)).await;
    let last = second_producer.batch(6..8);
    second_attempt.send(&last).await;
    second_attempt.send(&second_producer.end(8)).await;
    let acknowledgement = second_attempt.shut_down_and_read_until_closed().await;
    let (batches, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert!(
        matches!(first_batch.0.as_slice(), [Forwarded::Messages { .. }]),
        "{first_batch:?}"
    );
    assert_eq!(acknowledgement, ack(8));
    let forwarded: Vec<_> = batches.into_iter().flat_map(|batch| batch.0).collect();
    let [
        Forwarded::Suffix {
            num_duplicates,
            timestamps,
        },
        Forwarded::Messages { record_batch, .. },
    ] = forwarded.as_slice()
    else {
        panic!("unexpected forwarded batches: {forwarded:?}");
    };
    assert_eq!(*num_duplicates, 2);
    assert_eq!(timestamps, &[timestamp(4), timestamp(5)]);
    assert_eq!(record_batch, &split_messages(&last)[0]);
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: 8,
            num_duplicates_dropped: 4,
            num_incomplete_connections: 1,
            ..SessionStats::default()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlapping_attempts_with_other_batch_boundaries_emit_each_result_once() {
    let mut harness = Harness::start(BASE_SESSION_CONFIG, SearchResultEncoding).await;
    let mut first_attempt = harness.connect(3).await;
    let mut second_attempt = harness.connect(3).await;
    let mut first_producer = Producer::new(3, ARCHIVE_ID);
    let mut second_producer = Producer::new(3, ARCHIVE_ID);
    first_attempt.send(&first_producer.batch(0..2)).await;
    second_attempt.send(&second_producer.batch(0..3)).await;
    first_attempt.send(&first_producer.batch(2..5)).await;
    second_attempt.send(&second_producer.batch(3..9)).await;
    first_attempt.send(&first_producer.batch(5..10)).await;
    second_attempt.send(&second_producer.batch(9..10)).await;
    first_attempt.send(&first_producer.end(10)).await;
    second_attempt.send(&second_producer.end(10)).await;
    let (first_acknowledgement, second_acknowledgement) = tokio::join!(
        first_attempt.shut_down_and_read_until_closed(),
        second_attempt.shut_down_and_read_until_closed()
    );
    let (batches, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(first_acknowledgement, ack(10));
    assert_eq!(second_acknowledgement, ack(10));
    assert_eq!(sorted(batches.concat()), sorted(expected_results(3, 0..10)));
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: 10,
            num_duplicates_dropped: 10,
            ..SessionStats::default()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn v2_and_v3_attempts_of_one_task_share_its_cursor() {
    let mut harness = Harness::start(BASE_SESSION_CONFIG, SearchResultEncoding).await;
    let mut v3_attempt = harness.connect(0).await;
    let mut producer = Producer::new(0, ARCHIVE_ID);
    v3_attempt.send(&producer.batch(0..3)).await;
    let first_batch = harness.next_batch().await;
    drop(v3_attempt);

    let mut v2_attempt = harness.connect_with_version(0, 2).await;
    v2_attempt.send(&encode_v2_results(0, 0..5)).await;
    v2_attempt.send(&encode_trailer(5)).await;
    let acknowledgement = v2_attempt.shut_down_and_read_until_closed().await;
    let (batches, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(first_batch, expected_results(0, 0..3));
    assert_eq!(acknowledgement, ack(5));
    assert_eq!(batches.concat(), expected_results(0, 3..5));
    assert_eq!(outcome.stats.num_results_emitted, 5);
    assert_eq!(outcome.stats.num_duplicates_dropped, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acknowledgement_waits_until_the_last_batch_is_handed_to_the_consumer() {
    let session_config = SessionConfig {
        channel_size: NonZeroU32::MIN,
        ..BASE_SESSION_CONFIG
    };
    let mut harness = Harness::start(session_config, SearchResultEncoding).await;
    let mut task = harness.connect(0).await;
    let mut producer = Producer::new(0, ARCHIVE_ID);
    // Separate reads, so that the first batch fills the channel and the last waits to be sent.
    task.send(&producer.batch(0..5)).await;
    sleep(Duration::from_millis(100)).await;
    task.send(&producer.batch(5..10)).await;
    task.send(&producer.end(10)).await;
    task.shut_down().await;

    task.expect_open().await;
    let first_batch = harness.next_batch().await;
    let acknowledgement = task.read_until_closed().await;
    let (batches, _) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(first_batch, expected_results(0, 0..5));
    assert_eq!(acknowledgement, ack(10));
    assert_eq!(batches.concat(), expected_results(0, 5..10));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_consumer_stops_the_connection_from_reading() {
    /// Far more than the socket buffers of a loopback connection hold.
    const NUM_RESULTS: u64 = 1_000_000;
    const BATCH_NUM_RESULTS: u64 = 4096;

    let session_config = SessionConfig {
        channel_size: NonZeroU32::MIN,
        ..BASE_SESSION_CONFIG
    };
    let mut harness = Harness::start(session_config, SearchResultEncoding).await;
    let mut task = harness.connect(0).await;
    let mut producer = Producer::new(0, ARCHIVE_ID);
    let mut bytes = Vec::new();
    let mut begin = 0;
    while begin < NUM_RESULTS {
        let end = NUM_RESULTS.min(begin + BATCH_NUM_RESULTS);
        bytes.extend(producer.batch(begin..end));
        begin = end;
    }
    bytes.extend(producer.end(NUM_RESULTS));
    let mut task_handle = tokio::spawn(async move {
        task.send(&bytes).await;
        task.shut_down_and_read_until_closed().await
    });

    assert!(
        timeout(Duration::from_secs(1), &mut task_handle)
            .await
            .is_err(),
        "the listener shouldn't read every result while the consumer is stalled"
    );
    let mut results = Vec::new();
    while (results.len() as u64) < NUM_RESULTS {
        results.extend(harness.next_batch().await);
    }
    let acknowledgement = timeout(TIMEOUT, task_handle)
        .await
        .expect("the task should send every result once the consumer drains")
        .expect("the task shouldn't panic");
    let (remaining_batches, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(acknowledgement, ack(NUM_RESULTS));
    assert_eq!(remaining_batches.concat(), []);
    assert!(
        results == expected_results(0, 0..NUM_RESULTS),
        "every result should arrive once, in order"
    );
    assert_eq!(outcome.stats.num_results_emitted, NUM_RESULTS);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn protocol_violations_close_the_connection_without_an_acknowledgement() {
    for violation in [
        Violation::TrailerMismatch,
        Violation::DataAfterTrailer,
        Violation::ResultFrameAfterTheStream,
    ] {
        let mut harness = Harness::start(BASE_SESSION_CONFIG, SearchResultEncoding).await;
        let mut task = harness.connect(0).await;
        let mut producer = Producer::new(0, ARCHIVE_ID);
        task.send(&producer.batch(0..3)).await;
        task.send(&violation.end(producer)).await;
        let acknowledgement = task.shut_down_and_read_until_closed().await;
        let (batches, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

        assert_eq!(acknowledgement, [] as [u8; 0], "{violation:?}");
        assert_eq!(batches.concat(), expected_results(0, 0..3), "{violation:?}");
        assert_eq!(
            outcome.stats,
            SessionStats {
                num_results_emitted: 3,
                num_protocol_errors: 1,
                ..SessionStats::default()
            },
            "{violation:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dictionary_of_another_archive_is_a_protocol_error() {
    let mut harness = Harness::start(BASE_SESSION_CONFIG, SearchResultEncoding).await;
    let mut task = harness.connect(0).await;
    let mut producer = Producer::new(0, OTHER_ARCHIVE_ID);
    task.send(&producer.batch(0..3)).await;
    task.send(&producer.end(3)).await;
    let acknowledgement = task.shut_down_and_read_until_closed().await;
    let (batches, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(acknowledgement, [] as [u8; 0]);
    assert_eq!(batches.concat(), []);
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_protocol_errors: 1,
            ..SessionStats::default()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rust_sink_attempts_emit_each_result_once() {
    const NAME: &str = "search.ts";
    const KQL: &str = "*";

    let expected = expected_fixture_results(NAME, KQL);
    assert!(expected.len() > 20, "{}", expected.len());

    // A failed attempt streams its first batches, then a retry with other batch boundaries
    // streams every result.
    let mut harness = Harness::start(BASE_SESSION_CONFIG, SearchResultEncoding).await;
    let failed_attempt = Arc::new(FailingSink {
        inner: ArrowNetworkSink::new(destination(&harness.network_output, 5), row_limits(3)),
        failing_batch: 3,
    });
    let retry = Arc::new(ArrowNetworkSink::new(
        destination(&harness.network_output, 5),
        row_limits(4),
    ));
    let failed = tokio::task::spawn_blocking(move || {
        run_fixture_search(NAME, KQL, 5, failed_attempt).expect_err("the third batch fails")
    })
    .await
    .expect("the failed attempt shouldn't panic");
    let clp_s_reader::ReadError::Sink(sink_error) = &failed else {
        panic!("the failed attempt should fail in its sink: {failed:?}");
    };
    assert!(
        matches!(**sink_error, SinkError::Custom(_)),
        "{sink_error:?}"
    );
    let num_streamed = tokio::task::spawn_blocking(move || run_fixture_search(NAME, KQL, 7, retry))
        .await
        .expect("the retry shouldn't panic")
        .expect("the retry should be acknowledged");
    let (batches, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(num_streamed, expected.len() as u64);
    assert_eq!(
        batches.concat(),
        expected,
        "the results arrive once, in order"
    );
    assert_eq!(outcome.stats.num_results_emitted, expected.len() as u64);
    assert!(
        (1..expected.len() as u64).contains(&outcome.stats.num_duplicates_dropped),
        "the retry drops what the failed attempt streamed: {:?}",
        outcome.stats
    );
    assert_eq!(outcome.stats.num_incomplete_connections, 1);
    assert_eq!(outcome.stats.num_protocol_errors, 0);

    // Two concurrent attempts race on every batch.
    let mut harness = Harness::start(BASE_SESSION_CONFIG, SearchResultEncoding).await;
    let attempts: Vec<_> = [(3, 2), (5, 3)]
        .into_iter()
        .map(|(max_num_rows, batch_num_rows)| {
            let sink = Arc::new(ArrowNetworkSink::new(
                destination(&harness.network_output, 6),
                row_limits(max_num_rows),
            ));
            tokio::task::spawn_blocking(move || run_fixture_search(NAME, KQL, batch_num_rows, sink))
        })
        .collect();
    for attempt in attempts {
        assert_eq!(
            attempt
                .await
                .expect("the attempt shouldn't panic")
                .expect("the attempt should be acknowledged"),
            expected.len() as u64
        );
    }
    let (batches, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(sorted(batches.concat()), sorted(expected.clone()));
    assert_eq!(outcome.stats.num_results_emitted, expected.len() as u64);
    assert_eq!(outcome.stats.num_duplicates_dropped, expected.len() as u64);
}
