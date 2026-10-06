//! Tests that drive a [`ResultListener`] with simulated `clp-s` search tasks over real TCP
//! connections.

mod common;

use std::io::ErrorKind;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::time::Duration;

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
    _listener: ResultListener,
    network_output: NetworkOutput,
    status_source: FakeJobStatusSource,
    results: ResultStream,
    outcome: OutcomeFuture,
}

impl Harness {
    /// Binds a listener with `listener_config` and runs a session on it with `session_config`.
    async fn start(listener_config: ListenerConfig, session_config: SessionConfig) -> Self {
        let listener = ResultListener::bind(listener_config)
            .await
            .expect("binding a loopback listener should succeed");
        let session = listener.open_session(session_config);
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

    /// Connects a simulated attempt of a search task to the session.
    async fn connect(&self) -> FakeClpS {
        FakeClpS::connect(&self.network_output).await
    }

    /// # Returns
    ///
    /// The next result the session emits.
    async fn next_result(&mut self) -> SearchResult {
        timeout(TIMEOUT, self.results.next())
            .await
            .expect("a result should arrive")
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
        let outcome = timeout(TIMEOUT, &mut self.outcome)
            .await
            .expect("the outcome should resolve")
            .expect("the session should succeed");
        (results, outcome)
    }

    /// # Returns
    ///
    /// The remaining results, once the result stream has ended.
    async fn collect_results(&mut self) -> Vec<SearchResult> {
        timeout(TIMEOUT, (&mut self.results).collect::<Vec<_>>())
            .await
            .expect("the result stream should end")
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

    /// Sends a v1 handshake for task `task_index` of archive `archive_id`, presenting the token of
    /// `network_output`.
    async fn send_handshake(
        &mut self,
        network_output: &NetworkOutput,
        task_index: u64,
        archive_id: &str,
    ) {
        self.send(&encode_handshake(
            1,
            &network_output.session_token.to_string(),
            task_index,
            archive_id,
        ))
        .await;
    }

    /// Sends the results at `result_indices` of task `task_index` in a single write.
    async fn send_results(&mut self, task_index: u64, result_indices: Range<u64>) {
        let mut bytes = Vec::new();
        for result_index in result_indices {
            bytes.extend(encode_result(
                result_index,
                timestamp(result_index),
                &message(task_index, result_index),
            ));
        }
        self.send(&bytes).await;
    }

    /// Waits until the listener closes the connection without sending anything.
    async fn expect_closed_by_listener(mut self) {
        let mut byte = [0_u8; 1];
        match timeout(TIMEOUT, self.stream.read(&mut byte))
            .await
            .expect("the listener should close the connection")
        {
            Ok(0) => {}
            Ok(_) => panic!("the listener shouldn't send any bytes"),
            Err(e) => assert_eq!(
                e.kind(),
                ErrorKind::ConnectionReset,
                "unexpected error: {e}"
            ),
        }
    }
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
async fn single_attempt_emits_every_result_in_order() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut clp_s = harness.connect().await;
    clp_s
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    clp_s.send_results(0, 0..5).await;
    drop(clp_s);

    let (results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..5));
    assert_eq!(
        outcome,
        SessionOutcome {
            status: QueryJobStatus::Succeeded,
            stats: SessionStats {
                num_results_emitted: 5,
                ..SessionStats::default()
            },
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
    drop(retry);
    let (remaining_results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;
    results.extend(remaining_results);

    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..6));
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: 6,
            num_duplicates_dropped: 3,
            num_protocol_errors: 0,
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlapping_attempts_emit_each_result_once() {
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
    drop(first_attempt);
    drop(second_attempt);

    let (results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(
        sorted_messages(&results),
        sorted_messages(&expected_results(3, ARCHIVE_ID, 0..6))
    );
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: 6,
            num_duplicates_dropped: 6,
            num_protocol_errors: 0,
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn index_gap_closes_the_connection_without_moving_the_cursor() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut attempt = harness.connect().await;
    attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    attempt.send_results(0, 0..1).await;
    attempt.send_results(0, 2..3).await;
    attempt.expect_closed_by_listener().await;

    let mut retry = harness.connect().await;
    retry
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    retry.send_results(0, 0..4).await;
    drop(retry);
    let (results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..4));
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: 4,
            num_duplicates_dropped: 1,
            num_protocol_errors: 1,
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_session_token_is_rejected() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut stranger = harness.connect().await;
    stranger
        .send(&encode_handshake(
            1,
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
async fn unsupported_version_is_rejected() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut attempt = harness.connect().await;
    attempt
        .send(&encode_handshake(
            2,
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
    drop(retry);
    let (results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..3));
    assert_eq!(outcome.stats.num_duplicates_dropped, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_handshakes_are_rejected() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let session_token = harness.network_output.session_token.to_string();
    let mut not_an_array = Vec::new();
    rmp::encode::write_str(&mut not_an_array, &session_token)
        .expect("writing to a `Vec` shouldn't fail");
    let malformed_handshakes = [
        encode_handshake(1, "not-a-uuid", 0, ARCHIVE_ID),
        encode_handshake(1, &session_token, 0, "not-a-uuid"),
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
    drop(attempt);
    let (remaining_results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;
    results.extend(remaining_results);

    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..2));
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: 2,
            num_duplicates_dropped: 0,
            num_protocol_errors: 1,
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
            num_duplicates_dropped: 0,
            num_protocol_errors: 1,
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
    drop(attempt);

    let mut retry = harness.connect().await;
    retry
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    retry.send_results(0, 0..4).await;
    drop(retry);
    let (remaining_results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;
    results.extend(remaining_results);

    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..4));
    assert_eq!(
        outcome.stats,
        SessionStats {
            num_results_emitted: 4,
            num_duplicates_dropped: 2,
            num_protocol_errors: 0,
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claimed_result_reaches_a_slow_consumer_after_the_grace_period() {
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
    stale_attempt.send_results(0, 0..5).await;
    // Give the listener time to fill the channel and claim the next result.
    sleep(Duration::from_millis(200)).await;

    harness.status_source.set(Some(QueryJobStatus::Succeeded));
    sleep(session_config.drain_grace_period * 5).await;
    let (results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..5));
    assert_eq!(outcome.stats.num_results_emitted, 5);
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
    });
    sleep(Duration::from_millis(200)).await;

    harness.status_source.set(Some(QueryJobStatus::Succeeded));
    sleep(session_config.drain_grace_period * 2).await;
    let mut results = Vec::new();
    for _ in 0..NUM_FIRST_RESULTS {
        results.push(harness.next_result().await);
    }
    first_results_consumed_sender
        .send(())
        .expect("the attempt should wait for the signal");
    let (remaining_results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;
    results.extend(remaining_results);

    assert_eq!(results, expected_results(0, ARCHIVE_ID, 0..NUM_RESULTS));
    assert_eq!(outcome.stats.num_results_emitted, NUM_RESULTS);
    attempt_task
        .await
        .expect("the attempt should stream every result");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connection_opened_before_the_job_terminated_is_served() {
    let mut harness = Harness::start(BASE_LISTENER_CONFIG, BASE_SESSION_CONFIG).await;
    let mut attempt = harness.connect().await;
    sleep(Duration::from_millis(100)).await;

    harness.status_source.set(Some(QueryJobStatus::Succeeded));
    sleep(Duration::from_millis(300)).await;
    attempt
        .send_handshake(&harness.network_output, 0, ARCHIVE_ID)
        .await;
    attempt.send_results(0, 0..3).await;
    drop(attempt);
    let (results, outcome) = harness.finish(QueryJobStatus::Succeeded).await;

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
    drop(attempt);

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
    let session = listener.open_session(BASE_SESSION_CONFIG);
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
    let first_session = listener.open_session(BASE_SESSION_CONFIG);
    let second_session = listener.open_session(BASE_SESSION_CONFIG);
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
    drop(first_attempt);
    let mut second_attempt = FakeClpS::connect(&second_network_output).await;
    second_attempt
        .send_handshake(&second_network_output, 0, OTHER_ARCHIVE_ID)
        .await;
    second_attempt.send_results(0, 0..2).await;
    drop(second_attempt);
    status_source.set(Some(QueryJobStatus::Succeeded));

    let first_results = timeout(TIMEOUT, first_results.collect::<Vec<_>>())
        .await
        .expect("the first result stream should end");
    let second_results = timeout(TIMEOUT, second_results.collect::<Vec<_>>())
        .await
        .expect("the second result stream should end");
    assert_eq!(first_results, expected_results(0, ARCHIVE_ID, 0..3));
    assert_eq!(second_results, expected_results(0, OTHER_ARCHIVE_ID, 0..2));
    for outcome in [first_outcome, second_outcome] {
        let outcome = timeout(TIMEOUT, outcome)
            .await
            .expect("the outcome should resolve")
            .expect("the session should succeed");
        assert_eq!(outcome.stats.num_protocol_errors, 0);
    }
}
