//! Sessions, each of which collects the results streamed for one query job.

use std::future::Future;
use std::num::NonZeroU16;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use clp_rust_utils::job_config::NetworkOutput;
use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::job_config::QueryJobStatus;
use clp_rust_utils::job_config::SessionToken;
use clp_rust_utils::task_io::query::QueryTaskIndex;
use dashmap::DashMap;
use futures::Stream;
use non_empty_string::NonEmptyString;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::EncodedBatch;
use crate::Error;
use crate::JobStatusSource;
use crate::ResultEncoding;
use crate::channel;
use crate::cursor::TaskCursor;
use crate::listener::AcceptBarrier;

/// The sessions of a listener, keyed by their tokens.
pub type Registry<EncodingType> = DashMap<SessionToken, Arc<State<EncodingType>>>;

/// Per-session configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionConfig {
    /// The bytes of the batches buffered for the consumer, each holding the results a connection
    /// claimed from one socket read, before the connections stop reading from their sockets. A
    /// batch larger than this is buffered alone.
    pub channel_size: NonZeroU32,

    /// How long a connection may wait on its socket without claiming a result once its job has
    /// terminated, before the session stops reading from it. Once the job has succeeded, the
    /// session stops reading as soon as a connection waits at all, since every task of the job
    /// has had its results acknowledged.
    pub drain_grace_period: Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            channel_size: NonZeroU32::new(16 * 1024 * 1024)
                .expect("default channel size should not be zero"),
            drain_grace_period: Duration::from_secs(5),
        }
    }
}

/// Statistics of a session's connections.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SessionStats {
    /// The number of results emitted to the consumer.
    pub num_results_emitted: u64,

    /// The number of results dropped because another attempt of their task had already emitted
    /// them.
    pub num_duplicates_dropped: u64,

    /// The number of the session's connections closed because they violated the wire protocol.
    pub num_protocol_errors: u64,

    /// The number of the session's connections that ended without a protocol violation but before
    /// their results were acknowledged: they closed before their trailer, failed, or were cut off
    /// once the job terminated.
    pub num_incomplete_connections: u64,

    /// The number of claimed results discarded because the consumer had dropped the result stream.
    pub num_results_discarded: u64,
}

/// The outcome of a session whose query job has terminated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionOutcome {
    /// The query job's terminal status.
    pub status: QueryJobStatus,

    pub stats: SessionStats,
}

/// A session that receives the results streamed for one query job.
///
/// The session stays registered with its listener, accepting connections that present its token,
/// until its job terminates or it is dropped without being run.
///
/// # Type Parameters
///
/// * `EncodingType` - The encoding of the results the session delivers.
pub struct Session<EncodingType: ResultEncoding> {
    registration: Registration<EncodingType>,
    accept_barrier: AcceptBarrier,
    state: Arc<State<EncodingType>>,
    batches_receiver: channel::Receiver<EncodingType::Batch>,
    network_output: NetworkOutput,
}

impl<EncodingType: ResultEncoding> Session<EncodingType> {
    #[must_use]
    pub const fn token(&self) -> SessionToken {
        self.registration.token
    }

    /// # Returns
    ///
    /// The network output that directs a query job's search tasks to stream their results to this
    /// session.
    #[must_use]
    pub const fn network_output(&self) -> &NetworkOutput {
        &self.network_output
    }

    /// Runs the session for the query job `query_job_id` in a background task.
    ///
    /// The background task polls `status_source` until the job's status is terminal. It then stops
    /// accepting connections for the session, lets the open connections drain, ends the result
    /// stream, and resolves the outcome.
    ///
    /// # Type Parameters
    ///
    /// * `JobStatusSourceType` - The source of the query job's status.
    ///
    /// # Returns
    ///
    /// A tuple containing:
    ///
    /// * The stream of the job's results, in batches, which must be consumed for the session to
    ///   complete unless it is dropped. Dropping it doesn't stop the session, which then discards
    ///   the job's remaining results.
    /// * A future that resolves to the session's outcome once the stream has ended. Dropping it
    ///   doesn't stop the session.
    pub fn run<JobStatusSourceType: JobStatusSource + 'static>(
        self,
        query_job_id: QueryJobId,
        status_source: JobStatusSourceType,
    ) -> (ResultStream<EncodingType::Batch>, OutcomeFuture) {
        let Self {
            registration,
            accept_barrier,
            state,
            batches_receiver,
            ..
        } = self;
        let session_task = tokio::spawn(drive(
            registration,
            accept_barrier,
            state,
            query_job_id,
            status_source,
        ));
        (
            ResultStream { batches_receiver },
            OutcomeFuture { session_task },
        )
    }

    /// Factory function.
    ///
    /// Creates a session with a fresh token and registers it in `sessions`.
    ///
    /// # Returns
    ///
    /// The newly created session.
    ///
    /// # Panics
    ///
    /// Panics if the fresh token is already registered, which a random UUID rules out.
    pub(crate) fn open(
        sessions: Arc<Registry<EncodingType>>,
        accept_barrier: AcceptBarrier,
        advertised_host: NonEmptyString,
        port: NonZeroU16,
        config: SessionConfig,
        encoding: EncodingType,
    ) -> Self {
        let token = SessionToken::new_v4();
        let (batches_sender, batches_receiver) = channel::channel(config.channel_size);
        let state = Arc::new(State {
            cursors: DashMap::new(),
            encoding,
            batches_sender,
            draining: CancellationToken::new(),
            drain_grace_period: config.drain_grace_period,
            has_job_succeeded: AtomicBool::new(false),
            connections: TaskTracker::new(),
            has_joined_connection: AtomicBool::new(false),
            has_claimed_result: AtomicBool::new(false),
            num_results_emitted: AtomicU64::new(0),
            num_duplicates_dropped: AtomicU64::new(0),
            num_protocol_errors: AtomicU64::new(0),
            num_incomplete_connections: AtomicU64::new(0),
            num_results_discarded: AtomicU64::new(0),
        });
        assert!(
            sessions.insert(token, Arc::clone(&state)).is_none(),
            "a fresh session token should not be registered"
        );
        Self {
            registration: Registration { sessions, token },
            accept_barrier,
            state,
            batches_receiver,
            network_output: NetworkOutput {
                host: advertised_host,
                port,
                session_token: token,
            },
        }
    }
}

/// The stream of a session's results, in the batches its connections encoded them into.
///
/// The stream ends once the session's job has terminated and its connections have drained.
/// Dropping the stream earlier doesn't end the session.
///
/// # Type Parameters
///
/// * `BatchType` - The type of a batch of encoded results.
pub struct ResultStream<BatchType: EncodedBatch> {
    batches_receiver: channel::Receiver<BatchType>,
}

impl<BatchType: EncodedBatch> Stream for ResultStream<BatchType> {
    type Item = BatchType;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.batches_receiver.poll_recv(cx)
    }
}

/// A future that resolves to the outcome of a running session.
pub struct OutcomeFuture {
    session_task: JoinHandle<Result<SessionOutcome, Error>>,
}

impl Future for OutcomeFuture {
    type Output = Result<SessionOutcome, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.session_task)
            .poll(cx)
            .map(|joined| joined.map_err(Error::from).and_then(std::convert::identity))
    }
}

/// The state of a session shared with its connections.
///
/// # Type Parameters
///
/// * `EncodingType` - The encoding of the results the session delivers.
pub struct State<EncodingType: ResultEncoding> {
    pub cursors: DashMap<QueryTaskIndex, Arc<TaskCursor>>,
    pub encoding: EncodingType,
    pub batches_sender: channel::Sender<EncodingType::Batch>,

    /// Cancelled once the session's job has terminated and its connections should drain.
    pub draining: CancellationToken,

    pub connections: TaskTracker,
    drain_grace_period: Duration,

    /// Set before [`Self::draining`] is cancelled if the session's job succeeded.
    has_job_succeeded: AtomicBool,

    has_joined_connection: AtomicBool,
    has_claimed_result: AtomicBool,
    num_results_emitted: AtomicU64,
    num_duplicates_dropped: AtomicU64,
    num_protocol_errors: AtomicU64,
    num_incomplete_connections: AtomicU64,
    num_results_discarded: AtomicU64,
}

impl<EncodingType: ResultEncoding> State<EncodingType> {
    /// # Returns
    ///
    /// How long a connection may wait on its socket without claiming a result once the session
    /// drains: zero if the session's job succeeded, or the session's grace period otherwise.
    pub fn drain_grace_period(&self) -> Duration {
        if self.has_job_succeeded.load(Ordering::Acquire) {
            Duration::ZERO
        } else {
            self.drain_grace_period
        }
    }

    /// Records that a connection has joined the session.
    ///
    /// # Returns
    ///
    /// Whether the connection is the first to join the session.
    pub fn record_joined_connection(&self) -> bool {
        !self.has_joined_connection.swap(true, Ordering::Relaxed)
    }

    /// Records that a connection has claimed one of the session's results.
    ///
    /// # Returns
    ///
    /// Whether the result is the first that the session's connections have claimed.
    pub fn record_claimed_result(&self) -> bool {
        // The load keeps every claim after the first from writing to the shared flag.
        !self.has_claimed_result.load(Ordering::Relaxed)
            && !self.has_claimed_result.swap(true, Ordering::Relaxed)
    }

    /// Adds the statistics of a finished connection to the session's statistics.
    pub fn record(&self, stats: &SessionStats) {
        self.num_results_emitted
            .fetch_add(stats.num_results_emitted, Ordering::Relaxed);
        self.num_duplicates_dropped
            .fetch_add(stats.num_duplicates_dropped, Ordering::Relaxed);
        self.num_protocol_errors
            .fetch_add(stats.num_protocol_errors, Ordering::Relaxed);
        self.num_incomplete_connections
            .fetch_add(stats.num_incomplete_connections, Ordering::Relaxed);
        self.num_results_discarded
            .fetch_add(stats.num_results_discarded, Ordering::Relaxed);
    }

    /// # Returns
    ///
    /// The statistics recorded by the session's finished connections.
    fn stats(&self) -> SessionStats {
        SessionStats {
            num_results_emitted: self.num_results_emitted.load(Ordering::Relaxed),
            num_duplicates_dropped: self.num_duplicates_dropped.load(Ordering::Relaxed),
            num_protocol_errors: self.num_protocol_errors.load(Ordering::Relaxed),
            num_incomplete_connections: self.num_incomplete_connections.load(Ordering::Relaxed),
            num_results_discarded: self.num_results_discarded.load(Ordering::Relaxed),
        }
    }
}

/// Keeps a session registered with its listener until dropped.
///
/// # Type Parameters
///
/// * `EncodingType` - The encoding of the results the session delivers.
struct Registration<EncodingType: ResultEncoding> {
    sessions: Arc<Registry<EncodingType>>,
    token: SessionToken,
}

impl<EncodingType: ResultEncoding> Drop for Registration<EncodingType> {
    fn drop(&mut self) {
        self.sessions.remove(&self.token);
    }
}

/// Drives a running session: waits for its job to terminate, then shuts the session down.
///
/// # Type Parameters
///
/// * `EncodingType` - The encoding of the results the session delivers.
/// * `JobStatusSourceType` - The source of the query job's status.
///
/// # Returns
///
/// The session's outcome on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`wait_for_terminal_status`]'s return values on failure, after shutting the session
///   down.
async fn drive<EncodingType: ResultEncoding, JobStatusSourceType: JobStatusSource>(
    registration: Registration<EncodingType>,
    accept_barrier: AcceptBarrier,
    state: Arc<State<EncodingType>>,
    query_job_id: QueryJobId,
    status_source: JobStatusSourceType,
) -> Result<SessionOutcome, Error> {
    let status = wait_for_terminal_status(&status_source, query_job_id).await;

    // Tasks connect before they finish, so their connections have reached the socket by now.
    accept_barrier.wait().await;
    drop(registration);
    if matches!(status, Ok(QueryJobStatus::Succeeded)) {
        state.has_job_succeeded.store(true, Ordering::Release);
    }
    state.draining.cancel();
    state.connections.close();
    state.connections.wait().await;

    let session_stats = state.stats();
    // The connections have released their references, so dropping this one drops the batches
    // sender and ends the result stream.
    drop(state);

    Ok(SessionOutcome {
        status: status?,
        stats: session_stats,
    })
}

/// Polls `status_source` until the query job's status is terminal.
///
/// # Returns
///
/// The query job's terminal status on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`JobStatusSource::get_status`]'s return values on failure.
async fn wait_for_terminal_status(
    status_source: &impl JobStatusSource,
    query_job_id: QueryJobId,
) -> Result<QueryJobStatus, Error> {
    loop {
        let status = status_source.get_status(query_job_id).await?;
        if status.is_terminal() {
            return Ok(status);
        }
        tokio::time::sleep(status_source.poll_interval()).await;
    }
}
