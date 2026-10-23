//! Tests that drive a [`ResultListener`] with simulated `clp-s` search tasks over real TCP
//! connections.

mod common;

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::time::Duration;
use std::time::Instant;

use clp_rust_utils::job_config::NetworkOutput;
use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::job_config::QueryJobStatus;
use clp_rust_utils::types::ArchiveId;
use common::FakeJobStatusSource;
use futures::StreamExt;
use search_result_listener::Error;
use search_result_listener::ListenerConfig;
use search_result_listener::OutcomeFuture;
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
use uuid::Uuid;

const ARCHIVE_ID: &str = "018e90e5-8b2a-4a61-a2fc-cac799936caf";
const OTHER_ARCHIVE_ID: &str = "4b0c2f8e-6d13-4f4e-9a57-1c2d3e4f5a6b";
const QUERY_JOB_ID: QueryJobId = 7;
const PROTOCOL_VERSION: u64 = 2;

/// The longest a test waits for the listener before failing.
const TIMEOUT: Duration = Duration::from_secs(10);

const BASE_LISTENER_CONFIG: ListenerConfig = ListenerConfig {
    bind_addr: SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
    advertised_host: None,
    handshake_timeout: Duration::from_secs(10),
};

const BASE_SESSION_CONFIG: SessionConfig = SessionConfig {
    channel_capacity: NonZeroUsize::new(1024).expect("1024 is nonzero"),
    drain_grace_period: Duration::from_millis(200),
};

/// A loopback listener with one running session whose job status the test controls.
struct Harness {
    _listener: ResultListener<SearchResultEncoding>,
    network_output: NetworkOutput,
    status_source: FakeJobStatusSource,

    /// `None` once the test has dropped the result stream.
    results: Option<ResultStream<Vec<SearchResult>>>,

    /// The results of the received batches that the test hasn't taken yet.
    pending_results: VecDeque<SearchResult>,

    outcome: OutcomeFuture,
}

impl Harness {
    /// Binds a listener with `listener_config` and runs a session on it with `session_config`.
    async fn start(listener_config: ListenerConfig, session_config: SessionConfig) -> Self {
        let listener = ResultListener::bind(listener_config)
            .await
            .expect("binding a loopback listener should succeed");
        let session = listener.open_session(session_config, SearchResultEncoding);
        let network_output = session.network_output().clone();
        let status_source = FakeJobStatusSource::new();
        let (results, outcome) = session.run(QUERY_JOB_ID, status_source.clone());
        Self {
            _listener: listener,
            network_output,
            status_source,
            results: Some(results),
            pending_results: VecDeque::new(),
            outcome,
        }
    }

    /// Connects a simulated attempt of a search task to the session.
    async fn connect(&self) -> FakeClpS {
        FakeClpS::connect(&self.network_output).await
    }

    /// # Returns
    ///
    /// The next result the session emits.
    async fn next_result(&mut self) -> SearchResult {
        loop {
            if let Some(result) = self.pending_results.pop_front() {
                return result;
            }
            let batch = self.next_batch().await;
            self.pending_results.extend(batch);
        }
    }

    /// # Returns
    ///
    /// The next batch of results the session emits.
    ///
    /// # Panics
    ///
    /// Panics if results of an earlier batch haven't been taken yet.
    async fn next_batch(&mut self) -> Vec<SearchResult> {
        assert_eq!(
            self.pending_results,
            [],
            "the results of earlier batches should have been taken"
        );
        timeout(TIMEOUT, self.results().next())
            .await
            .expect("a batch should arrive")
            .expect("the result stream shouldn't end yet")
    }

    /// Marks the job as terminated with `status`.
    ///
    /// # Returns
    ///
    /// The results emitted after the ones already received, and the session's outcome.
    async fn finish(&mut self, status: QueryJobStatus) -> (Vec<SearchResult>, SessionOutcome) {
        self.status_source.set(Some(status));
        let results = self.collect_results().await;
        (results, self.outcome().await)
    }

    /// # Returns
    ///
    /// The remaining results, once the result stream has ended.
    async fn collect_results(&mut self) -> Vec<SearchResult> {
        let mut results: Vec<_> = self.pending_results.drain(..).collect();
        results.extend(
            timeout(TIMEOUT, self.results().concat())
                .await
                .expect("the result stream should end"),
        );
        results
    }

    /// # Returns
    ///
    /// The session's outcome, once it resolves.
    async fn outcome(&mut self) -> SessionOutcome {
        timeout(TIMEOUT, &mut self.outcome)
            .await
            .expect("the outcome should resolve")
            .expect("the session should succeed")
    }

    /// # Returns
    ///
    /// The session's result stream.
    ///
    /// # Panics
    ///
    /// Panics if the test has dropped the result stream.
    const fn results(&mut self) -> &mut ResultStream<Vec<SearchResult>> {
        self.results
            .as_mut()
            .expect("the result stream shouldn't have been dropped")
    }
}

/// A simulated attempt of a `clp-s` search task.
struct FakeClpS {
    stream: TcpStream,
}

impl FakeClpS {
    /// Connects to the listener named by `network_output`.
    async fn connect(network_output: &NetworkOutput) -> Self {
        let stream = TcpStream::connect((network_output.host.as_str(), network_output.port.get()))
            .await
            .expect("connecting to the listener should succeed");
        Self { stream }
    }

    /// Sends `bytes` as they are.
    async fn send(&mut self, bytes: &[u8]) {
        self.stream
            .write_all(bytes)
            .await
            .expect("sending to the listener should succeed");
    }

    /// Sends a v2 handshake for task `task_index` of archive `archive_id`, presenting the token of
    /// `network_output`.
    async fn send_handshake(
        &mut self,
        network_output: &NetworkOutput,
        task_index: u64,
        archive_id: &str,
    ) {
        self.send(&encode_handshake(
            PROTOCOL_VERSION,
            &network_output.session_token.to_string(),
            task_index,
            archive_id,
        ))
        .await;
    }

    /// Sends the results at `result_indices` of task `task_index` in a single write.
    async fn send_results(
        &mut self,
        task_index: u64,
        result_indices: impl IntoIterator<Item = u64>,
    ) {
        self.send(&encode_results(task_index, result_indices)).await;
    }

    /// Sends a trailer counting `num_results` results.
    async fn send_trailer(&mut self, num_results: u64) {
        self.send(&encode_trailer(num_results)).await;
    }

    /// Shuts down the sending side of the connection, as `clp-s` does after its trailer.
    async fn shut_down_write(&mut self) {
        self.stream
            .shutdown()
            .await
            .expect("sending EOF to the listener should succeed");
    }

    /// Ends the stream the way `clp-s` does: sends a trailer counting `num_results` results, then
    /// EOF, and waits until the listener closes the connection.
    ///
    /// # Returns
    ///
    /// The bytes the listener sent back.
    async fn finish(mut self, num_results: u64) -> Vec<u8> {
        self.send_trailer(num_results).await;
        self.shut_down_write().await;
        self.read_until_closed().await
    }

    /// Sends the results at `result_indices` of task `task_index`, one every `interval`, until the
    /// listener closes the connection or every result is sent.
    async fn send_results_slowly_until_closed(
        mut self,
        task_index: u64,
        result_indices: Range<u64>,
        interval: Duration,
    ) {
        for result_index in result_indices {
            if self
                .stream
                .write_all(&encode_results(task_index, [result_index]))
                .await
                .is_err()
            {
                return;
            }
            sleep(interval).await;
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

    /// Waits until the listener closes the connection without sending anything.
    async fn expect_closed_by_listener(self) {
        assert_eq!(
            self.read_until_closed().await,
            [] as [u8; 0],
            "the listener shouldn't acknowledge the connection"
        );
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
}

/// # Returns
///
/// The acknowledgement of a connection that streamed `num_results` results: the msgpack array
/// `[num_results]` with the count encoded as a `uint 64`.
fn ack(num_results: u64) -> Vec<u8> {
    let mut bytes = vec![0x91, 0xcf];
    bytes.extend_from_slice(&num_results.to_be_bytes());
    bytes
}

/// # Returns
///
/// The bytes of a handshake with the given fields.
fn encode_handshake(
    version: u64,
    session_token: &str,
    task_index: u64,
    archive_id: &str,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    rmp::encode::write_array_len(&mut bytes, 4).expect("writing to a `Vec` shouldn't fail");
    rmp::encode::write_uint(&mut bytes, version).expect("writing to a `Vec` shouldn't fail");
    rmp::encode::write_str(&mut bytes, session_token).expect("writing to a `Vec` shouldn't fail");
    rmp::encode::write_uint(&mut bytes, task_index).expect("writing to a `Vec` shouldn't fail");
    rmp::encode::write_str(&mut bytes, archive_id).expect("writing to a `Vec` shouldn't fail");
    bytes
}

/// # Returns
///
/// The bytes of a result frame with the given fields.
fn encode_result(result_index: u64, timestamp: i64, message: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    rmp::encode::write_array_len(&mut bytes, 3).expect("writing to a `Vec` shouldn't fail");
    rmp::encode::write_uint(&mut bytes, result_index).expect("writing to a `Vec` shouldn't fail");
    rmp::encode::write_sint(&mut bytes, timestamp).expect("writing to a `Vec` shouldn't fail");
    rmp::encode::write_str(&mut bytes, message).expect("writing to a `Vec` shouldn't fail");
    bytes
}

/// # Returns
///
/// The bytes of the result frames at `result_indices` of task `task_index`.
fn encode_results(task_index: u64, result_indices: impl IntoIterator<Item = u64>) -> Vec<u8> {
    let mut bytes = Vec::new();
    for result_index in result_indices {
        bytes.extend(encode_result(
            result_index,
            timestamp(result_index),
            &message(task_index, result_index),
        ));
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
/// The timestamp of the result at `result_index`.
fn timestamp(result_index: u64) -> i64 {
    1_700_000_000_000 + i64::try_from(result_index).expect("test result indices fit in `i64`")
}

/// # Returns
///
/// The message of the result at `result_index` of task `task_index`.
fn message(task_index: u64, result_index: u64) -> String {
    format!("{{\"task\":{task_index},\"result\":{result_index}}}\n")
}

/// # Returns
///
/// The results at `result_indices` of task `task_index` of archive `archive_id`, in index order.
fn expected_results(
    task_index: u64,
    archive_id: &str,
    result_indices: Range<u64>,
) -> Vec<SearchResult> {
    let archive_id = archive_id
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
/// The messages of `results`, sorted.
fn sorted_messages(results: &[SearchResult]) -> Vec<String> {
    let mut messages: Vec<_> = results
        .iter()
        .map(|result| result.message.clone())
        .collect();
    messages.sort_unstable();
    messages
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_attempt_emits_every_result_in_order_and_is_acknowledged() {
    const NUM_RESULTS: u64 = 20_000;

    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut clp_s = harness.connect().await;
    clp_s
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    clp_s.send_results(0, 0..NUM_RESULTS).await;
    let acknowledgement = clp_s.finish(NUM_RESULTS).await;

    let (results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(
        acknowledgement,
        [0x91, 0xcf, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x4e, 0x20]
    );
    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..NUM_RESULTS));
    assert_eq!(
        outcome,
        SessionOutcome {
            status: QueryJobStatus::Succeeded,
            stats: SessionStats {
                num_results_emitted: NUM_RESULTS,
                ..SessionStats::default()
            },
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acknowledgement_waits_until_the_last_batch_is_handed_to_the_consumer() {
    let session_config = SessionConfig {
        channel_capacity: NonZeroUsize::MIN,
        ..BASE_SESSION_CONFIG
    };
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, session_config).await;
    let mut attempt = harness.connect().await;
    attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    // Separate reads, so that the first batch fills the channel and the last waits to be sent.
    attempt.send_results(0, 0..5).await;
    sleep(Duration::from_millis(100)).await;
    attempt.send_results(0, 5..10).await;
    attempt.send_trailer(10).await;
    attempt.shut_down_write().await;

    attempt.expect_open().await;
    let first_batch = harness.next_batch().await;
    let acknowledgement = attempt.read_until_closed().await;
    let (remaining_results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(first_batch, expected_results(0, ARCHIVE_ID, 0..5));
    assert_eq!(acknowledgement, ack(10));
    assert_eq!(remaining_results, expected_results(0, ARCHIVE_ID, 5..10));
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: 10,
            ..SessionStats::default()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequential_retry_drops_the_results_already_emitted() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut first_attempt = harness.connect().await;
    first_attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    first_attempt.send_results(0, 0..3).await;
    let mut results = Vec::new();
    for _ in 0..3 {
        results.push(harness.next_result().await);
    }
    drop(first_attempt);

    let mut retry = harness.connect().await;
    retry
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    retry.send_results(0, 0..6).await;
    let acknowledgement = retry.finish(6).await;
    let (remaining_results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;
    results.extend(remaining_results);

    assert_eq!(acknowledgement, ack(6));
    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..6));
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: 6,
            num_duplicates_dropped: 3,
            num_protocol_errors: 0,
            num_incomplete_connections: 1,
            num_results_discarded: 0,
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_of_only_duplicates_is_acknowledged() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut first_attempt = harness.connect().await;
    first_attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    first_attempt.send_results(0, 0..3).await;
    let first_acknowledgement = first_attempt.finish(3).await;

    let mut retry = harness.connect().await;
    retry
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    retry.send_results(0, 0..3).await;
    let retry_acknowledgement = retry.finish(3).await;
    let (results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(first_acknowledgement, ack(3));
    assert_eq!(retry_acknowledgement, ack(3));
    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..3));
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: 3,
            num_duplicates_dropped: 3,
            ..SessionStats::default()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlapping_attempts_emit_each_result_once_and_are_both_acknowledged() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut first_attempt = harness.connect().await;
    let mut second_attempt = harness.connect().await;
    first_attempt
        .send_handshake(&harness.network_output, 3, ARCHIVE_ID)
        .await;
    second_attempt
        .send_handshake(&harness.network_output, 3, ARCHIVE_ID)
        .await;
    first_attempt.send_results(3, 0..2).await;
    second_attempt.send_results(3, 0..3).await;
    first_attempt.send_results(3, 2..4).await;
    second_attempt.send_results(3, 3..6).await;
    first_attempt.send_results(3, 4..6).await;
    let (first_acknowledgement, second_acknowledgement) =
        tokio::join!(first_attempt.finish(6), second_attempt.finish(6));

    let (results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(first_acknowledgement, ack(6));
    assert_eq!(second_acknowledgement, ack(6));
    assert_eq!(
        sorted_messages(&results),
        sorted_messages(&expected_results(3, ARCHIVE_ID, 0..6))
    );
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: 6,
            num_duplicates_dropped: 6,
            ..SessionStats::default()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn out_of_order_index_closes_the_connection_without_moving_the_cursor() {
    for out_of_order_indices in [[0, 2], [0, 0]] {
        let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
        let mut attempt = harness.connect().await;
        attempt
            .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
            .await;
        attempt.send_results(0, out_of_order_indices).await;
        attempt.expect_closed_by_listener().await;

        let mut retry = harness.connect().await;
        retry
            .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
            .await;
        retry.send_results(0, 0..4).await;
        let acknowledgement = retry.finish(4).await;
        let (results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

        assert_eq!(acknowledgement, ack(4), "{out_of_order_indices:?}");
        assert_eq!(
            results,
            expected_results(0, ARCHIVE_ID, 0..4),
            "{out_of_order_indices:?}"
        );
        assert_eq!(
            outcome.stats,
            SessionStats {
                num_results_emitted: 4,
                num_duplicates_dropped: 1,
                num_protocol_errors: 1,
                ..SessionStats::default()
            },
            "{out_of_order_indices:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claimed_results_before_an_out_of_order_index_in_the_same_read_are_emitted() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut attempt = harness.connect().await;
    attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    attempt.send_results(0, [0, 1, 3]).await;
    attempt.expect_closed_by_listener().await;

    let mut retry = harness.connect().await;
    retry
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    retry.send_results(0, 0..4).await;
    let acknowledgement = retry.finish(4).await;
    let (results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(acknowledgement, ack(4));
    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..4));
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: 4,
            num_duplicates_dropped: 2,
            num_protocol_errors: 1,
            ..SessionStats::default()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trailer_counting_other_results_is_a_protocol_error() {
    for trailer_num_results in [2, 4] {
        let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
        let mut attempt = harness.connect().await;
        attempt
            .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
            .await;
        attempt.send_results(0, 0..3).await;
        let acknowledgement = attempt.finish(trailer_num_results).await;
        let (results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

        assert_eq!(acknowledgement, [] as [u8; 0], "{trailer_num_results}");
        assert_eq!(
            results,
            expected_results(0, ARCHIVE_ID, 0..3),
            "{trailer_num_results}"
        );
        assert_eq!(
            outcome.stats,
            SessionStats {
                num_results_emitted: 3,
                num_protocol_errors: 1,
                ..SessionStats::default()
            },
            "{trailer_num_results}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bytes_after_the_trailer_are_a_protocol_error() {
    for is_same_write in [true, false] {
        let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
        let mut attempt = harness.connect().await;
        attempt
            .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
            .await;
        let mut bytes = encode_results(0, 0..2);
        bytes.extend(encode_trailer(2));
        if is_same_write {
            bytes.extend(encode_results(0, [2]));
            attempt.send(&bytes).await;
        } else {
            attempt.send(&bytes).await;
            sleep(Duration::from_millis(100)).await;
            attempt.send(&[0x00]).await;
        }
        attempt.shut_down_write().await;
        let acknowledgement = attempt.read_until_closed().await;
        let (results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

        assert_eq!(acknowledgement, [] as [u8; 0], "{is_same_write}");
        assert_eq!(
            results,
            expected_results(0, ARCHIVE_ID, 0..2),
            "{is_same_write}"
        );
        assert_eq!(
            outcome.stats,
            SessionStats {
                num_results_emitted: 2,
                num_protocol_errors: 1,
                ..SessionStats::default()
            },
            "{is_same_write}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eof_without_a_trailer_is_not_acknowledged() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut attempt = harness.connect().await;
    attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    attempt.send_results(0, 0..3).await;
    attempt.shut_down_write().await;
    attempt.expect_closed_by_listener().await;

    let (results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..3));
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: 3,
            num_incomplete_connections: 1,
            ..SessionStats::default()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn result_cut_off_by_a_disconnect_is_never_claimed() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut attempt = harness.connect().await;
    attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    attempt.send_results(0, 0..2).await;
    let cut_off_result = encode_result(2, timestamp(2), &message(0, 2));
    attempt
        .send(&cut_off_result[..cut_off_result.len() - 3])
        .await;
    let mut results = vec![harness.next_result().await, harness.next_result().await];
    attempt.shut_down_write().await;
    attempt.expect_closed_by_listener().await;

    let mut retry = harness.connect().await;
    retry
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    retry.send_results(0, 0..4).await;
    let acknowledgement = retry.finish(4).await;
    let (remaining_results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;
    results.extend(remaining_results);

    assert_eq!(acknowledgement, ack(4));
    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..4));
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: 4,
            num_duplicates_dropped: 2,
            num_protocol_errors: 0,
            num_incomplete_connections: 1,
            num_results_discarded: 0,
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_session_token_is_rejected() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut stranger = harness.connect().await;
    stranger
        .send(&encode_handshake(
            PROTOCOL_VERSION,
            &Uuid::new_v4().to_string(),
            0,
            ARCHIVE_ID,
        ))
        .await;
    stranger.send_results(0, 0..3).await;
    stranger.expect_closed_by_listener().await;

    let (results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(results, []);
    assert_eq!(outcome.stats, SessionStats::default());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn version_1_handshake_is_rejected() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut attempt = harness.connect().await;
    attempt
        .send(&encode_handshake(
            1,
            &harness.network_output.session_token.to_string(),
            0,
            ARCHIVE_ID,
        ))
        .await;
    attempt.send_results(0, 0..3).await;
    attempt.expect_closed_by_listener().await;

    let mut retry = harness.connect().await;
    retry
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    retry.send_results(0, 0..3).await;
    let acknowledgement = retry.finish(3).await;
    let (results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(acknowledgement, ack(3));
    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..3));
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: 3,
            ..SessionStats::default()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_handshakes_are_rejected() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let session_token = harness.network_output.session_token.to_string();
    let mut not_an_array = Vec::new();
    rmp::encode::write_str(&mut not_an_array, &session_token)
        .expect("writing to a `Vec` shouldn't fail");
    let malformed_handshakes = [
        encode_handshake(PROTOCOL_VERSION, "not-a-uuid", 0, ARCHIVE_ID),
        encode_handshake(PROTOCOL_VERSION, &session_token, 0, "not-a-uuid"),
        not_an_array,
    ];

    for handshake in malformed_handshakes {
        let mut attempt = harness.connect().await;
        attempt.send(&handshake).await;
        attempt.send_results(0, 0..3).await;
        attempt.expect_closed_by_listener().await;
    }
    let (results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(results, []);
    assert_eq!(outcome.stats, SessionStats::default());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn archive_id_mismatch_is_rejected() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut attempt = harness.connect().await;
    attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    attempt.send_results(0, 0..2).await;
    let mut results = vec![harness.next_result().await, harness.next_result().await];

    let mut mismatched = harness.connect().await;
    mismatched
        .send_handshake(&harness.network_output, 0, OTHER_ARCHIVE_ID)
        .await;
    mismatched.send_results(0, 0..4).await;
    mismatched.expect_closed_by_listener().await;
    let acknowledgement = attempt.finish(2).await;
    let (remaining_results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;
    results.extend(remaining_results);

    assert_eq!(acknowledgement, ack(2));
    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..2));
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: 2,
            num_protocol_errors: 1,
            ..SessionStats::default()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_result_frame_closes_the_connection() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut attempt = harness.connect().await;
    attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    attempt.send_results(0, 0..2).await;
    let mut two_fields = Vec::new();
    rmp::encode::write_array_len(&mut two_fields, 2).expect("writing to a `Vec` shouldn't fail");
    rmp::encode::write_uint(&mut two_fields, 2).expect("writing to a `Vec` shouldn't fail");
    rmp::encode::write_str(&mut two_fields, "message\n")
        .expect("writing to a `Vec` shouldn't fail");
    attempt.send(&two_fields).await;
    attempt.expect_closed_by_listener().await;

    let (results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..2));
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: 2,
            num_protocol_errors: 1,
            ..SessionStats::default()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claimed_results_before_a_malformed_frame_in_the_same_read_are_emitted() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut attempt = harness.connect().await;
    attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    let mut bytes = encode_results(0, 0..2);
    rmp::encode::write_str(&mut bytes, "not a result frame")
        .expect("writing to a `Vec` shouldn't fail");
    attempt.send(&bytes).await;
    attempt.expect_closed_by_listener().await;

    let (results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..2));
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: 2,
            num_protocol_errors: 1,
            ..SessionStats::default()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn results_of_one_read_are_delivered_in_few_batches() {
    const MAX_NUM_BATCHES: usize = 10;
    const NUM_RESULTS: u64 = 1000;

    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut attempt = harness.connect().await;
    attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    attempt.send_results(0, 0..NUM_RESULTS).await;
    assert_eq!(attempt.finish(NUM_RESULTS).await, ack(NUM_RESULTS));
    harness.status_source.set(Some(QueryJobStatus::Succeeded));
    let batches = timeout(TIMEOUT, harness.results().collect::<Vec<_>>())
        .await
        .expect("the result stream should end");

    assert!(
        batches.len() <= MAX_NUM_BATCHES,
        "the results arrived in {} batches",
        batches.len()
    );
    assert_eq!(
        batches.concat(),
        expected_results(0, ARCHIVE_ID, 0..NUM_RESULTS)
    );
    assert_eq!(
        harness.outcome().await.stats.num_results_emitted,
        NUM_RESULTS
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_consumer_stops_the_connection_from_reading() {
    /// Far more than the socket buffers of a loopback connection hold.
    const NUM_BYTES_SENT: usize = 64 * 1024 * 1024;
    const MESSAGE_LEN: usize = 4096;
    const NUM_RESULTS: u64 = (NUM_BYTES_SENT / MESSAGE_LEN) as u64;

    let session_config = SessionConfig {
        channel_capacity: NonZeroUsize::MIN,
        ..BASE_SESSION_CONFIG
    };
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, session_config).await;
    let mut attempt = harness.connect().await;
    attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    let large_message =
        |result_index: u64| format!("{result_index:08}{}\n", "x".repeat(MESSAGE_LEN - 9));
    let mut bytes = Vec::with_capacity(NUM_BYTES_SENT + 1024 * 1024);
    for result_index in 0..NUM_RESULTS {
        bytes.extend(encode_result(
            result_index,
            timestamp(result_index),
            &large_message(result_index),
        ));
    }
    let mut attempt_task = tokio::spawn(async move {
        attempt.send(&bytes).await;
        attempt.finish(NUM_RESULTS).await
    });

    assert!(
        timeout(Duration::from_secs(1), &mut attempt_task)
            .await
            .is_err(),
        "the listener shouldn't read every result while the consumer is stalled"
    );
    let mut results = Vec::new();
    for _ in 0..NUM_RESULTS {
        results.push(harness.next_result().await);
    }
    let acknowledgement = timeout(TIMEOUT, attempt_task)
        .await
        .expect("the attempt should send every result once the consumer drains")
        .expect("the attempt shouldn't panic");
    let (remaining_results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;
    results.extend(remaining_results);

    let archive_id = ARCHIVE_ID
        .parse::<ArchiveId>()
        .expect("test archive IDs are valid UUIDs");
    assert!(
        results
            == (0..NUM_RESULTS)
                .map(|result_index| SearchResult {
                    archive_id,
                    timestamp: timestamp(result_index),
                    message: large_message(result_index),
                })
                .collect::<Vec<_>>(),
        "every result should arrive once, in order"
    );
    assert_eq!(acknowledgement, ack(NUM_RESULTS));
    assert_eq!(outcome.stats.num_results_emitted, NUM_RESULTS);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claimed_batch_reaches_a_slow_consumer_after_the_job_succeeds() {
    let session_config = SessionConfig {
        channel_capacity: NonZeroUsize::MIN,
        ..BASE_SESSION_CONFIG
    };
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, session_config).await;
    // A stale attempt that keeps its connection open after the job has terminated.
    let mut stale_attempt = harness.connect().await;
    stale_attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    // Separate reads, so that the first batch fills the channel and the second waits to be sent.
    stale_attempt.send_results(0, 0..5).await;
    sleep(Duration::from_millis(100)).await;
    stale_attempt.send_results(0, 5..10).await;
    sleep(Duration::from_millis(100)).await;

    harness.status_source.set(Some(QueryJobStatus::Succeeded));
    sleep(session_config.drain_grace_period * 5).await;
    let (results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..10));
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: 10,
            num_incomplete_connections: 1,
            ..SessionStats::default()
        }
    );
    stale_attempt.expect_closed_by_listener().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn results_still_arriving_reach_a_slow_consumer_after_the_grace_period() {
    const NUM_RESULTS: u64 = 4000;
    const NUM_FIRST_RESULTS: u64 = NUM_RESULTS / 2;

    let session_config = SessionConfig {
        channel_capacity: NonZeroUsize::MIN,
        drain_grace_period: Duration::from_millis(500),
    };
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, session_config).await;
    let mut attempt = harness.connect().await;
    let network_output = harness.network_output.clone();
    let (first_results_consumed_sender, first_results_consumed_receiver) =
        tokio::sync::oneshot::channel();
    // The first results exceed the listener's read buffer, so some of them wait in the socket
    // while the consumer is stalled. The rest arrive only after the listener has run out of data,
    // with a delay shorter than the grace period.
    let attempt_task = tokio::spawn(async move {
        attempt.send_handshake(&network_output, 0, ARCHIVE_ID).await;
        attempt.send_results(0, 0..NUM_FIRST_RESULTS).await;
        first_results_consumed_receiver
            .await
            .expect("the test should signal once it consumed the first results");
        sleep(Duration::from_millis(50)).await;
        attempt
            .send_results(0, NUM_FIRST_RESULTS..NUM_RESULTS)
            .await;
        attempt.finish(NUM_RESULTS).await
    });
    sleep(Duration::from_millis(200)).await;

    harness.status_source.set(Some(QueryJobStatus::Failed));
    sleep(session_config.drain_grace_period * 2).await;
    let mut results = Vec::new();
    for _ in 0..NUM_FIRST_RESULTS {
        results.push(harness.next_result().await);
    }
    first_results_consumed_sender
        .send(())
        .expect("the attempt should wait for the signal");
    let (remaining_results, outcome) = harness.finish(QueryJobStatus::Failed).await;
    results.extend(remaining_results);

    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..NUM_RESULTS));
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: NUM_RESULTS,
            ..SessionStats::default()
        }
    );
    assert_eq!(
        attempt_task
            .await
            .expect("the attempt should stream every result"),
        ack(NUM_RESULTS)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connection_without_a_trailer_is_cut_off_after_the_grace_period() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut attempt = harness.connect().await;
    attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    attempt.send_results(0, 0..3).await;

    let (results, outcome) = harness.finish(QueryJobStatus::Failed).await;

    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..3));
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: 3,
            num_incomplete_connections: 1,
            ..SessionStats::default()
        }
    );
    attempt.expect_closed_by_listener().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_attempt_sending_duplicates_is_cut_off_after_the_grace_period() {
    const NUM_RESULTS: u64 = 40;

    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut stale_attempt = harness.connect().await;
    stale_attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    let mut attempt = harness.connect().await;
    attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    attempt.send_results(0, 0..NUM_RESULTS).await;
    assert_eq!(attempt.finish(NUM_RESULTS).await, ack(NUM_RESULTS));
    let mut results = Vec::new();
    for _ in 0..NUM_RESULTS {
        results.push(harness.next_result().await);
    }
    // Sending every duplicate takes ten grace periods.
    let stale_attempt_task = tokio::spawn(stale_attempt.send_results_slowly_until_closed(
        0,
        0..NUM_RESULTS,
        BASE_SESSION_CONFIG.drain_grace_period / 4,
    ));

    let finish_started_at = Instant::now();
    let (remaining_results, outcome) = harness.finish(QueryJobStatus::Failed).await;
    let finish_duration = finish_started_at.elapsed();
    results.extend(remaining_results);

    assert!(
        finish_duration < BASE_SESSION_CONFIG.drain_grace_period * 5,
        "the session took {finish_duration:?} to finish"
    );
    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..NUM_RESULTS));
    assert_eq!(outcome.stats.num_results_emitted, NUM_RESULTS);
    assert_eq!(outcome.stats.num_incomplete_connections, 1);
    timeout(TIMEOUT, stale_attempt_task)
        .await
        .expect("the listener should close the stale attempt's connection")
        .expect("the stale attempt shouldn't panic");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn succeeded_job_stops_reading_its_stale_connections_at_once() {
    let session_config = SessionConfig {
        drain_grace_period: Duration::from_mins(1),
        ..BASE_SESSION_CONFIG
    };
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, session_config).await;
    let mut attempt = harness.connect().await;
    attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    attempt.send_results(0, 0..3).await;
    assert_eq!(attempt.finish(3).await, ack(3));
    let mut stale_attempt = harness.connect().await;
    stale_attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    stale_attempt.send_results(0, 0..2).await;
    sleep(Duration::from_millis(100)).await;

    let finish_started_at = Instant::now();
    let (results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;
    let finish_duration = finish_started_at.elapsed();

    assert!(
        finish_duration < Duration::from_secs(2),
        "the session took {finish_duration:?} to finish"
    );
    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..3));
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: 3,
            num_duplicates_dropped: 2,
            num_incomplete_connections: 1,
            ..SessionStats::default()
        }
    );
    stale_attempt.expect_closed_by_listener().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connection_opened_before_the_job_terminated_is_served() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut attempt = harness.connect().await;
    sleep(Duration::from_millis(100)).await;

    harness.status_source.set(Some(QueryJobStatus::Failed));
    sleep(Duration::from_millis(300)).await;
    attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    attempt.send_results(0, 0..3).await;
    let acknowledgement = attempt.finish(3).await;
    let (results, outcome) = harness.finish(QueryJobStatus::Failed).await;

    assert_eq!(acknowledgement, ack(3));
    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..3));
    assert_eq!(outcome.stats.num_results_emitted, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connection_without_a_handshake_is_closed_after_the_handshake_timeout() {
    let listener_config = ListenerConfig {
        handshake_timeout: Duration::from_millis(200),
        ..BASE_LISTENER_CONFIG
    };
    let mut harness = Harness::start(listener_config, BASE_SESSION_CONFIG).await;
    let silent = harness.connect().await;
    silent.expect_closed_by_listener().await;

    let (results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(results, []);
    assert_eq!(outcome.status, QueryJobStatus::Succeeded);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_connection_after_the_session_ended_is_rejected() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let (results, _) = harness.finish(QueryJobStatus::Succeeded).await;
    assert_eq!(results, []);

    let mut late_attempt = harness.connect().await;
    late_attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    late_attempt.send_results(0, 0..3).await;
    late_attempt.expect_closed_by_listener().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outcome_reports_a_failed_job() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;

    let (results, outcome) = harness.finish(QueryJobStatus::Failed).await;

    assert_eq!(results, []);
    assert_eq!(
        outcome,
        SessionOutcome {
            status: QueryJobStatus::Failed,
            stats: SessionStats::default(),
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_poll_failure_ends_the_session_with_an_error() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut attempt = harness.connect().await;
    attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    attempt.send_results(0, 0..2).await;
    let mut results = vec![harness.next_result().await, harness.next_result().await];
    assert_eq!(attempt.finish(2).await, ack(2));

    harness.status_source.set(None);
    results.extend(harness.collect_results().await);
    let outcome = timeout(TIMEOUT, &mut harness.outcome)
        .await
        .expect("the outcome should resolve");

    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..2));
    assert!(
        matches!(outcome, Err(Error::QueryJobNotFound(QUERY_JOB_ID))),
        "unexpected outcome: {outcome:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_a_session_without_running_it_unregisters_it() {
    let listener = ResultListener::bind(BASE_LISTENER_CONFIG)
        .await
        .expect("binding a loopback listener should succeed");
    let session = listener.open_session(BASE_SESSION_CONFIG, SearchResultEncoding);
    let network_output = session.network_output().clone();
    drop(session);

    let mut attempt = FakeClpS::connect(&network_output).await;
    attempt.send_handshake(&network_output, 0, ARCHIVE_ID).await;
    attempt.expect_closed_by_listener().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_on_one_listener_receive_only_their_own_results() {
    let listener = ResultListener::bind(BASE_LISTENER_CONFIG)
        .await
        .expect("binding a loopback listener should succeed");
    let first_session = listener.open_session(BASE_SESSION_CONFIG, SearchResultEncoding);
    let second_session = listener.open_session(BASE_SESSION_CONFIG, SearchResultEncoding);
    let first_network_output = first_session.network_output().clone();
    let second_network_output = second_session.network_output().clone();
    let status_source = FakeJobStatusSource::new();
    let (first_results, first_outcome) = first_session.run(QUERY_JOB_ID, status_source.clone());
    let (second_results, second_outcome) =
        second_session.run(QUERY_JOB_ID + 1, status_source.clone());

    let mut first_attempt = FakeClpS::connect(&first_network_output).await;
    first_attempt
        .send_handshake(&first_network_output, 0, ARCHIVE_ID)
        .await;
    first_attempt.send_results(0, 0..3).await;
    assert_eq!(first_attempt.finish(3).await, ack(3));
    let mut second_attempt = FakeClpS::connect(&second_network_output).await;
    second_attempt
        .send_handshake(&second_network_output, 0, OTHER_ARCHIVE_ID)
        .await;
    second_attempt.send_results(0, 0..2).await;
    assert_eq!(second_attempt.finish(2).await, ack(2));
    status_source.set(Some(QueryJobStatus::Succeeded));

    let first_results = timeout(TIMEOUT, first_results.concat())
        .await
        .expect("the first result stream should end");
    let second_results = timeout(TIMEOUT, second_results.concat())
        .await
        .expect("the second result stream should end");
    assert_eq!(first_results, expected_results(0, ARCHIVE_ID, 0..3));
    assert_eq!(second_results, expected_results(0, OTHER_ARCHIVE_ID, 0..2));
    for (outcome, num_results) in [(first_outcome, 3), (second_outcome, 2)] {
        let outcome = timeout(TIMEOUT, outcome)
            .await
            .expect("the outcome should resolve")
            .expect("the session should succeed");
        assert_eq!(
            outcome.stats,
            SessionStats {
                num_results_emitted: num_results,
                ..SessionStats::default()
            }
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn results_after_the_result_stream_is_dropped_are_discarded_and_acknowledged() {
    const NUM_RESULTS: u64 = 1000;
    const NUM_RESULTS_RECEIVED: u64 = 2;

    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut attempt = harness.connect().await;
    attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    attempt.send_results(0, 0..NUM_RESULTS_RECEIVED).await;
    let mut results = Vec::new();
    for _ in 0..NUM_RESULTS_RECEIVED {
        results.push(harness.next_result().await);
    }
    harness.results = None;

    attempt
        .send_results(0, NUM_RESULTS_RECEIVED..NUM_RESULTS)
        .await;
    attempt.expect_open().await;
    let acknowledgement = attempt.finish(NUM_RESULTS).await;
    harness.status_source.set(Some(QueryJobStatus::Succeeded));
    let outcome = harness.outcome().await;

    assert_eq!(acknowledgement, ack(NUM_RESULTS));
    assert_eq!(
        results,
        expected_results(0, ARCHIVE_ID, 0..NUM_RESULTS_RECEIVED)
    );
    assert_eq!(
        outcome,
        SessionOutcome {
            status: QueryJobStatus::Succeeded,
            stats: SessionStats {
                num_results_emitted: NUM_RESULTS_RECEIVED,
                num_duplicates_dropped: 0,
                num_protocol_errors: 0,
                num_incomplete_connections: 0,
                num_results_discarded: NUM_RESULTS - NUM_RESULTS_RECEIVED,
            },
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connection_blocked_on_a_full_channel_keeps_reading_once_the_result_stream_is_dropped() {
    const NUM_RESULTS_PER_WRITE: u64 = 10;
    const NUM_WRITES: u64 = 3;

    let session_config = SessionConfig {
        channel_capacity: NonZeroUsize::MIN,
        ..BASE_SESSION_CONFIG
    };
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, session_config).await;
    let mut attempt = harness.connect().await;
    attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    // Separate reads: the first batch fills the channel, the second waits to be sent, and the third
    // waits in the socket.
    for write_index in 0..NUM_WRITES {
        attempt
            .send_results(
                0,
                write_index * NUM_RESULTS_PER_WRITE..(write_index + 1) * NUM_RESULTS_PER_WRITE,
            )
            .await;
        sleep(Duration::from_millis(100)).await;
    }

    harness.results = None;
    attempt.expect_open().await;
    let acknowledgement = attempt.finish(NUM_WRITES * NUM_RESULTS_PER_WRITE).await;
    harness.status_source.set(Some(QueryJobStatus::Succeeded));
    let outcome = harness.outcome().await;

    assert_eq!(acknowledgement, ack(NUM_WRITES * NUM_RESULTS_PER_WRITE));
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: NUM_RESULTS_PER_WRITE,
            num_duplicates_dropped: 0,
            num_protocol_errors: 0,
            num_incomplete_connections: 0,
            num_results_discarded: (NUM_WRITES - 1) * NUM_RESULTS_PER_WRITE,
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_the_result_stream_keeps_the_session_open_until_the_job_terminates() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    harness.results = None;

    let mut attempt = harness.connect().await;
    attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    attempt.send_results(0, 0..3).await;
    attempt.expect_open().await;
    let acknowledgement = attempt.finish(3).await;
    let mut retry = harness.connect().await;
    retry
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    retry.send_results(0, 0..5).await;
    retry.expect_open().await;
    let retry_acknowledgement = retry.finish(5).await;
    harness.status_source.set(Some(QueryJobStatus::Succeeded));
    let outcome = harness.outcome().await;

    assert_eq!(acknowledgement, ack(3));
    assert_eq!(retry_acknowledgement, ack(5));
    assert_eq!(
        outcome,
        SessionOutcome {
            status: QueryJobStatus::Succeeded,
            stats: SessionStats {
                num_results_emitted: 0,
                num_duplicates_dropped: 3,
                num_protocol_errors: 0,
                num_incomplete_connections: 0,
                num_results_discarded: 5,
            },
        }
    );
    let mut late_attempt = harness.connect().await;
    late_attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    late_attempt.send_results(0, 0..3).await;
    late_attempt.expect_closed_by_listener().await;
}
