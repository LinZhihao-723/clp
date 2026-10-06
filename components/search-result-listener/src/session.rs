//! Sessions, each of which collects the results streamed for one query job.

use std::future::Future;
use std::num::NonZeroU16;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use clp_rust_utils::job_config::NetworkOutput;
use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::job_config::QueryJobStatus;
use clp_rust_utils::task_io::query::QueryTaskIndex;
use clp_rust_utils::types::ArchiveId;
use dashmap::DashMap;
use futures::Stream;
use non_empty_string::NonEmptyString;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::Error;
use crate::JobStatusSource;
use crate::SessionToken;
use crate::cursor::TaskCursor;
use crate::listener::AcceptBarrier;

/// The sessions of a listener, keyed by their tokens.
pub type Registry = DashMap<SessionToken, Arc<State>>;

/// Per-session configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionConfig {
    /// The number of results buffered for the consumer before the connections stop reading from
    /// their sockets.
    pub channel_capacity: NonZeroUsize,

    /// How long a connection may wait on its socket without claiming a result once its job has
    /// terminated, before the session stops reading from it.
    pub drain_grace_period: Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            channel_capacity: NonZeroUsize::new(1024)
                .expect("default channel capacity should not be zero"),
            drain_grace_period: Duration::from_secs(5),
        }
    }
}

/// A search result emitted by a session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchResult {
    pub archive_id: ArchiveId,
    pub timestamp: i64,
    pub message: String,
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
pub struct Session {
    registration: Registration,
    accept_barrier: AcceptBarrier,
    state: Arc<State>,
    results_receiver: mpsc::Receiver<SearchResult>,
    network_output: NetworkOutput,
}

impl Session {
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
    /// * `StatusSource` - The source of the query job's status.
    ///
    /// # Returns
    ///
    /// A tuple containing:
    ///
    /// * The stream of the job's results, which must be consumed for the session to complete.
    /// * A future that resolves to the session's outcome once the stream has ended. Dropping it
    ///   doesn't stop the session.
    pub fn run<StatusSource: JobStatusSource + 'static>(
        self,
        query_job_id: QueryJobId,
        status_source: StatusSource,
    ) -> (ResultStream, OutcomeFuture) {
        let Self {
            registration,
            accept_barrier,
            state,
            results_receiver,
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
            ResultStream { results_receiver },
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
        sessions: &Arc<Registry>,
        accept_barrier: AcceptBarrier,
        advertised_host: NonEmptyString,
        port: NonZeroU16,
        config: SessionConfig,
    ) -> Self {
        let token = SessionToken::new_v4();
        let (results_sender, results_receiver) = mpsc::channel(config.channel_capacity.get());
        let state = Arc::new(State {
            cursors: DashMap::new(),
            results_sender,
            draining: CancellationToken::new(),
            drain_grace_period: config.drain_grace_period,
            connections: TaskTracker::new(),
            num_results_emitted: AtomicU64::new(0),
            num_duplicates_dropped: AtomicU64::new(0),
            num_protocol_errors: AtomicU64::new(0),
        });
        assert!(
            sessions.insert(token, Arc::clone(&state)).is_none(),
            "a fresh session token should not be registered"
        );
        Self {
            registration: Registration {
                sessions: Arc::clone(sessions),
                token,
            },
            accept_barrier,
            state,
            results_receiver,
            network_output: NetworkOutput {
                host: advertised_host,
                port,
                session_token: token,
            },
        }
    }
}

/// The stream of a session's results.
///
/// The stream ends once the session's job has terminated and its connections have drained.
pub struct ResultStream {
    results_receiver: mpsc::Receiver<SearchResult>,
}

impl Stream for ResultStream {
    type Item = SearchResult;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.results_receiver.poll_recv(cx)
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
pub struct State {
    pub cursors: DashMap<QueryTaskIndex, Arc<TaskCursor>>,
    pub results_sender: mpsc::Sender<SearchResult>,

    /// Cancelled once the session's job has terminated and its connections should drain.
    pub draining: CancellationToken,

    pub drain_grace_period: Duration,
    pub connections: TaskTracker,
    num_results_emitted: AtomicU64,
    num_duplicates_dropped: AtomicU64,
    num_protocol_errors: AtomicU64,
}

impl State {
    /// Adds the statistics of a finished connection to the session's statistics.
    pub fn record(&self, stats: &SessionStats) {
        self.num_results_emitted
            .fetch_add(stats.num_results_emitted, Ordering::Relaxed);
        self.num_duplicates_dropped
            .fetch_add(stats.num_duplicates_dropped, Ordering::Relaxed);
        self.num_protocol_errors
            .fetch_add(stats.num_protocol_errors, Ordering::Relaxed);
    }

    /// # Returns
    ///
    /// The statistics recorded by the session's finished connections.
    fn stats(&self) -> SessionStats {
        SessionStats {
            num_results_emitted: self.num_results_emitted.load(Ordering::Relaxed),
            num_duplicates_dropped: self.num_duplicates_dropped.load(Ordering::Relaxed),
            num_protocol_errors: self.num_protocol_errors.load(Ordering::Relaxed),
        }
    }
}

/// Keeps a session registered with its listener until dropped.
struct Registration {
    sessions: Arc<Registry>,
    token: SessionToken,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.sessions.remove(&self.token);
    }
}

/// Drives a running session: waits for its job to terminate, then shuts the session down.
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
async fn drive<StatusSource: JobStatusSource>(
    registration: Registration,
    accept_barrier: AcceptBarrier,
    state: Arc<State>,
    query_job_id: QueryJobId,
    status_source: StatusSource,
) -> Result<SessionOutcome, Error> {
    let status = wait_for_terminal_status(&status_source, query_job_id).await;

    // Tasks connect before they finish, so their connections have reached the socket by now.
    accept_barrier.wait().await;
    drop(registration);
    state.draining.cancel();
    state.connections.close();
    state.connections.wait().await;

    let session_stats = state.stats();
    // The connections have released their references, so dropping this one drops the results
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
