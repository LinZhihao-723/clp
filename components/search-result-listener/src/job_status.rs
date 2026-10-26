//! Sources of query job statuses, which a session waits on to learn when its job has terminated.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use clp_rust_utils::job_config::QUERY_JOBS_TABLE_NAME;
use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::job_config::QueryJobStatus;
use const_format::formatcp;
use tokio::sync::oneshot;

use crate::Error;

/// How long polls of a [`PolledJobStatusSource`] may keep failing before the waits fail.
pub const POLL_FAILURE_TOLERANCE: Duration = Duration::from_secs(1);

/// A source of query job statuses.
#[async_trait]
pub trait JobStatusSource: Send + Sync {
    /// Waits until a query job's status is terminal.
    ///
    /// # Parameters
    ///
    /// * `query_job_id` - The ID of the query job.
    ///
    /// # Returns
    ///
    /// The query job's terminal status on success.
    ///
    /// # Errors
    ///
    /// Implementations must document their error conditions.
    async fn wait_for_terminal_status(
        &self,
        query_job_id: QueryJobId,
    ) -> Result<QueryJobStatus, Error>;
}

/// A table that holds the statuses of query jobs.
#[async_trait]
pub trait JobStatusTable: Send + Sync + 'static {
    /// Fetches the current statuses of query jobs.
    ///
    /// # Parameters
    ///
    /// * `query_job_ids` - The distinct IDs of the query jobs, of which there is at least one.
    ///
    /// # Returns
    ///
    /// The status of each of the query jobs whose row exists, on success.
    ///
    /// # Errors
    ///
    /// Implementations must document their error conditions.
    async fn get_statuses(
        &self,
        query_job_ids: &[QueryJobId],
    ) -> Result<HashMap<QueryJobId, QueryJobStatus>, sqlx::Error>;
}

/// A [`JobStatusTable`] that reads statuses from the query jobs table of the CLP database.
pub struct MariaDbJobStatusTable {
    db_pool: sqlx::MySqlPool,
}

impl MariaDbJobStatusTable {
    #[must_use]
    pub const fn new(db_pool: sqlx::MySqlPool) -> Self {
        Self { db_pool }
    }
}

#[async_trait]
impl JobStatusTable for MariaDbJobStatusTable {
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`sqlx::query::QueryAs::fetch_all`]'s return values on failure.
    async fn get_statuses(
        &self,
        query_job_ids: &[QueryJobId],
    ) -> Result<HashMap<QueryJobId, QueryJobStatus>, sqlx::Error> {
        const MAX_NUM_IDS_PER_QUERY: usize = 1000;

        let mut statuses = HashMap::with_capacity(query_job_ids.len());
        for chunk in query_job_ids.chunks(MAX_NUM_IDS_PER_QUERY) {
            let mut query_builder = sqlx::QueryBuilder::<sqlx::MySql>::new(formatcp!(
                "SELECT `id`, `status` FROM `{QUERY_JOBS_TABLE_NAME}` WHERE `id` IN ("
            ));
            let mut separated_ids = query_builder.separated(", ");
            for query_job_id in chunk {
                separated_ids.push_bind(query_job_id);
            }
            query_builder.push(")");
            statuses.extend(
                query_builder
                    .build_query_as::<(QueryJobId, QueryJobStatus)>()
                    .fetch_all(&self.db_pool)
                    .await?,
            );
        }
        Ok(statuses)
    }
}

/// The [`JobStatusSource`] of the query jobs table of the CLP database.
pub type MariaDbJobStatusSource = PolledJobStatusSource<MariaDbJobStatusTable>;

/// A [`JobStatusSource`] that polls a [`JobStatusTable`] in a background task, fetching the
/// statuses of every query job waited on through the source or its clones at once, until no job is
/// waited on.
///
/// # Type Parameters
///
/// * `JobStatusTableType` - The table that holds the query jobs' statuses.
pub struct PolledJobStatusSource<JobStatusTableType: JobStatusTable> {
    poller: Arc<Poller<JobStatusTableType>>,
}

impl<JobStatusTableType: JobStatusTable> PolledJobStatusSource<JobStatusTableType> {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A source that polls `table`, with `poll_interval` between two consecutive polls.
    #[must_use]
    pub fn new(table: JobStatusTableType, poll_interval: Duration) -> Self {
        Self {
            poller: Arc::new(Poller {
                table,
                poll_interval,
                state: Mutex::new(PollerState::default()),
            }),
        }
    }
}

impl<JobStatusTableType: JobStatusTable> Clone for PolledJobStatusSource<JobStatusTableType> {
    fn clone(&self) -> Self {
        Self {
            poller: Arc::clone(&self.poller),
        }
    }
}

#[async_trait]
impl<JobStatusTableType: JobStatusTable> JobStatusSource
    for PolledJobStatusSource<JobStatusTableType>
{
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Error::QueryJobNotFound`] if the query job's row doesn't exist.
    /// * [`Error::JobStatusPoll`] if polls keep failing for [`POLL_FAILURE_TOLERANCE`] before the
    ///   query job's status is terminal. A poll that succeeds within the tolerance hides the
    ///   failures before it.
    ///
    /// # Panics
    ///
    /// Panics if the background task panics before the query job's status is terminal.
    async fn wait_for_terminal_status(
        &self,
        query_job_id: QueryJobId,
    ) -> Result<QueryJobStatus, Error> {
        let (sender, receiver) = oneshot::channel();
        self.poller.add_waiter(query_job_id, sender);
        receiver
            .await
            .expect("the status poller should resolve every wait before it stops")
    }
}

/// The state shared by a [`PolledJobStatusSource`], its clones and its background task.
///
/// # Type Parameters
///
/// * `JobStatusTableType` - The table that holds the query jobs' statuses.
struct Poller<JobStatusTableType: JobStatusTable> {
    table: JobStatusTableType,
    poll_interval: Duration,
    state: Mutex<PollerState>,
}

impl<JobStatusTableType: JobStatusTable> Poller<JobStatusTableType> {
    /// Adds a waiter on `query_job_id`, which `sender` resolves, and starts the background task
    /// unless it runs.
    fn add_waiter(
        self: &Arc<Self>,
        query_job_id: QueryJobId,
        sender: oneshot::Sender<Result<QueryJobStatus, Error>>,
    ) {
        let mut state = self.lock_state();
        state.waiters.entry(query_job_id).or_default().push(sender);
        let is_polling = std::mem::replace(&mut state.is_polling, true);
        drop(state);
        if !is_polling {
            tokio::spawn(Arc::clone(self).run());
        }
    }

    /// Polls the table until no query job is waited on. Each poll resolves the waits on every query
    /// job whose status is terminal or whose row doesn't exist. Once polls have kept failing for
    /// [`POLL_FAILURE_TOLERANCE`], the failing poll resolves every wait. If the task stops for any
    /// other reason, such as a panic, it drops every wait's sender.
    async fn run(self: Arc<Self>) {
        let mut guard = RunGuard {
            poller: &self,
            is_stopped: false,
        };
        let mut first_failure_time: Option<Instant> = None;
        loop {
            let query_job_ids: Vec<QueryJobId> = {
                let mut state = self.lock_state();
                state.waiters.retain(|_, senders| {
                    senders.retain(|sender| !sender.is_closed());
                    !senders.is_empty()
                });
                if state.waiters.is_empty() {
                    state.is_polling = false;
                    guard.is_stopped = true;
                    return;
                }
                state.waiters.keys().copied().collect()
            };

            match self.table.get_statuses(&query_job_ids).await {
                Ok(statuses) => {
                    first_failure_time = None;
                    let mut resolved = Vec::new();
                    let mut state = self.lock_state();
                    for query_job_id in query_job_ids {
                        let status = match statuses.get(&query_job_id) {
                            Some(status) if !status.is_terminal() => continue,
                            Some(&status) => Some(status),
                            None => None,
                        };
                        if let Some(senders) = state.waiters.remove(&query_job_id) {
                            resolved.push((query_job_id, status, senders));
                        }
                    }
                    drop(state);
                    for (query_job_id, status, senders) in resolved {
                        for sender in senders {
                            // The send fails only if the wait was dropped.
                            let _ =
                                sender.send(status.ok_or(Error::QueryJobNotFound(query_job_id)));
                        }
                    }
                }
                Err(error) => {
                    let failing_since = *first_failure_time.get_or_insert_with(Instant::now);
                    if failing_since.elapsed() < POLL_FAILURE_TOLERANCE {
                        tracing::warn!(
                            error = % error,
                            "Failed to poll the statuses of the query jobs being waited on; \
                             retrying."
                        );
                    } else {
                        tracing::error!(
                            error = % error,
                            "Failed to poll the statuses of the query jobs being waited on; \
                             failing every wait."
                        );
                        first_failure_time = None;
                        let error = Arc::new(error);
                        let waiters = std::mem::take(&mut self.lock_state().waiters);
                        for sender in waiters.into_values().flatten() {
                            let _ = sender.send(Err(Error::JobStatusPoll(Arc::clone(&error))));
                        }
                    }
                }
            }

            tokio::time::sleep(self.poll_interval).await;
        }
    }

    /// # Returns
    ///
    /// The locked state.
    ///
    /// # Panics
    ///
    /// Panics if the state lock is poisoned.
    fn lock_state(&self) -> MutexGuard<'_, PollerState> {
        self.state
            .lock()
            .expect("the status poller's state lock shouldn't be poisoned")
    }
}

/// The waits that a [`Poller`] resolves.
#[derive(Default)]
struct PollerState {
    /// The senders that resolve the waits on each query job.
    waiters: HashMap<QueryJobId, Vec<oneshot::Sender<Result<QueryJobStatus, Error>>>>,

    /// Whether the background task runs. The task clears it, with the state locked, once no query
    /// job is waited on.
    is_polling: bool,
}

/// Clears the state of a [`Poller`] whose background task stops without having stopped polling,
/// which drops the senders of every wait and lets the next wait start a new task.
///
/// # Type Parameters
///
/// * `JobStatusTableType` - The table that holds the query jobs' statuses.
struct RunGuard<'poller, JobStatusTableType: JobStatusTable> {
    poller: &'poller Poller<JobStatusTableType>,
    is_stopped: bool,
}

impl<JobStatusTableType: JobStatusTable> Drop for RunGuard<'_, JobStatusTableType> {
    fn drop(&mut self) {
        if self.is_stopped {
            return;
        }
        let mut state = self
            .poller
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let waiters = std::mem::take(&mut state.waiters);
        state.is_polling = false;
        drop(state);
        drop(waiters);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::MutexGuard;
    use std::time::Duration;
    use std::time::Instant;

    use async_trait::async_trait;
    use clp_rust_utils::job_config::QueryJobId;
    use clp_rust_utils::job_config::QueryJobStatus;
    use tokio::task::JoinHandle;
    use tokio::time::sleep;
    use tokio::time::timeout;

    use super::JobStatusSource;
    use super::JobStatusTable;
    use super::POLL_FAILURE_TOLERANCE;
    use super::PolledJobStatusSource;
    use crate::Error;

    const POLL_INTERVAL: Duration = Duration::from_millis(5);

    /// A [`JobStatusTable`] whose rows the test sets, and that records the IDs of every fetch.
    #[derive(Clone, Default)]
    struct FakeTable {
        state: Arc<Mutex<FakeTableState>>,
    }

    impl FakeTable {
        /// # Returns
        ///
        /// A table whose rows are `statuses`.
        fn new(statuses: &[(QueryJobId, QueryJobStatus)]) -> Self {
            let table = Self::default();
            table.lock().statuses.extend(statuses.iter().copied());
            table
        }

        fn set(&self, query_job_id: QueryJobId, status: QueryJobStatus) {
            self.lock().statuses.insert(query_job_id, status);
        }

        /// # Returns
        ///
        /// The sorted IDs of each fetch so far.
        fn fetches(&self) -> Vec<Vec<QueryJobId>> {
            self.lock().fetches.clone()
        }

        /// # Returns
        ///
        /// The locked state.
        ///
        /// # Panics
        ///
        /// Panics if the state lock is poisoned.
        fn lock(&self) -> MutexGuard<'_, FakeTableState> {
            self.state
                .lock()
                .expect("the table lock shouldn't be poisoned")
        }
    }

    #[async_trait]
    impl JobStatusTable for FakeTable {
        async fn get_statuses(
            &self,
            query_job_ids: &[QueryJobId],
        ) -> Result<HashMap<QueryJobId, QueryJobStatus>, sqlx::Error> {
            let mut sorted_ids = query_job_ids.to_vec();
            sorted_ids.sort_unstable();
            let mut state = self.lock();
            state.fetches.push(sorted_ids);
            if state.should_panic {
                drop(state);
                panic!("the fake table panics when asked to");
            }
            if state.should_fail {
                return Err(sqlx::Error::PoolTimedOut);
            }
            let statuses = query_job_ids
                .iter()
                .filter_map(|id| state.statuses.get(id).map(|status| (*id, *status)))
                .collect();
            drop(state);
            Ok(statuses)
        }
    }

    #[derive(Default)]
    struct FakeTableState {
        statuses: HashMap<QueryJobId, QueryJobStatus>,
        should_fail: bool,
        should_panic: bool,
        fetches: Vec<Vec<QueryJobId>>,
    }

    /// Waits on `query_job_id` through `source` in a background task.
    ///
    /// # Returns
    ///
    /// The handle of the background task.
    fn spawn_wait(
        source: &PolledJobStatusSource<FakeTable>,
        query_job_id: QueryJobId,
    ) -> JoinHandle<Result<QueryJobStatus, Error>> {
        let source = source.clone();
        tokio::spawn(async move { source.wait_for_terminal_status(query_job_id).await })
    }

    /// # Returns
    ///
    /// The result of the wait of `wait`.
    ///
    /// # Panics
    ///
    /// Panics if the wait doesn't end within five seconds or its task panics.
    async fn join(
        wait: JoinHandle<Result<QueryJobStatus, Error>>,
    ) -> Result<QueryJobStatus, Error> {
        timeout(Duration::from_secs(5), wait)
            .await
            .expect("the wait should end")
            .expect("the waiting task shouldn't panic")
    }

    #[tokio::test]
    async fn waits_on_several_jobs_share_each_poll() -> anyhow::Result<()> {
        let table = FakeTable::new(&[(1, QueryJobStatus::Running), (2, QueryJobStatus::Pending)]);
        let source = PolledJobStatusSource::new(table.clone(), POLL_INTERVAL);
        let [first, second, third] = [
            spawn_wait(&source, 1),
            spawn_wait(&source, 2),
            spawn_wait(&source.clone(), 2),
        ];
        sleep(POLL_INTERVAL * 10).await;
        table.set(2, QueryJobStatus::Running);
        sleep(POLL_INTERVAL * 4).await;

        table.set(1, QueryJobStatus::Succeeded);
        table.set(2, QueryJobStatus::Failed);
        assert_eq!(join(first).await?, QueryJobStatus::Succeeded);
        assert_eq!(join(second).await?, QueryJobStatus::Failed);
        assert_eq!(join(third).await?, QueryJobStatus::Failed);

        let fetches = table.fetches();
        assert!(fetches.len() >= 3, "{fetches:?}");
        assert!(
            fetches.iter().skip(1).all(|ids| ids == &[1, 2]),
            "every poll after the waits started should fetch both jobs once: {fetches:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn missing_job_fails_only_its_own_waits() -> anyhow::Result<()> {
        let table = FakeTable::new(&[(1, QueryJobStatus::Running)]);
        let source = PolledJobStatusSource::new(table.clone(), POLL_INTERVAL);
        let present = spawn_wait(&source, 1);
        let missing = spawn_wait(&source, 2);

        assert!(matches!(
            join(missing).await,
            Err(Error::QueryJobNotFound(2))
        ));
        sleep(POLL_INTERVAL * 4).await;
        assert!(!present.is_finished());

        table.set(1, QueryJobStatus::Cancelled);
        assert_eq!(join(present).await?, QueryJobStatus::Cancelled);
        Ok(())
    }

    #[tokio::test]
    async fn polls_failing_within_the_tolerance_are_retried() -> anyhow::Result<()> {
        let table = FakeTable::new(&[(1, QueryJobStatus::Running)]);
        let source = PolledJobStatusSource::new(table.clone(), POLL_INTERVAL);
        let wait = spawn_wait(&source, 1);
        sleep(POLL_INTERVAL * 4).await;

        table.lock().should_fail = true;
        sleep(POLL_FAILURE_TOLERANCE / 2).await;
        assert!(!wait.is_finished());
        table.lock().should_fail = false;
        sleep(POLL_INTERVAL * 4).await;
        table.lock().should_fail = true;
        sleep(POLL_FAILURE_TOLERANCE / 2 + POLL_INTERVAL * 4).await;
        assert!(!wait.is_finished());

        table.lock().should_fail = false;
        table.set(1, QueryJobStatus::Succeeded);
        assert_eq!(join(wait).await?, QueryJobStatus::Succeeded);
        Ok(())
    }

    #[tokio::test]
    async fn polls_failing_beyond_the_tolerance_fail_every_wait() {
        let table = FakeTable::new(&[(1, QueryJobStatus::Running), (2, QueryJobStatus::Running)]);
        let source = PolledJobStatusSource::new(table.clone(), POLL_INTERVAL);
        let waits = [spawn_wait(&source, 1), spawn_wait(&source, 2)];
        sleep(POLL_INTERVAL * 4).await;

        table.lock().should_fail = true;
        let failure_time = Instant::now();
        for wait in waits {
            assert!(matches!(join(wait).await, Err(Error::JobStatusPoll(_))));
        }
        assert!(failure_time.elapsed() >= POLL_FAILURE_TOLERANCE);
    }

    #[tokio::test]
    async fn panicking_poll_ends_every_wait_and_polling_restarts() -> anyhow::Result<()> {
        let table = FakeTable::new(&[(1, QueryJobStatus::Running), (2, QueryJobStatus::Running)]);
        let source = PolledJobStatusSource::new(table.clone(), POLL_INTERVAL);
        let waits = [spawn_wait(&source, 1), spawn_wait(&source, 2)];
        sleep(POLL_INTERVAL * 4).await;

        table.lock().should_panic = true;
        for wait in waits {
            let join_error = timeout(Duration::from_secs(5), wait)
                .await
                .expect("the wait should end")
                .expect_err("the wait should panic");
            assert!(join_error.is_panic());
        }

        table.lock().should_panic = false;
        table.set(1, QueryJobStatus::Succeeded);
        assert_eq!(
            join(spawn_wait(&source, 1)).await?,
            QueryJobStatus::Succeeded
        );
        Ok(())
    }

    #[tokio::test]
    async fn polling_stops_without_waits_and_resumes_for_a_new_one() -> anyhow::Result<()> {
        let table = FakeTable::new(&[(1, QueryJobStatus::Running), (2, QueryJobStatus::Running)]);
        let source = PolledJobStatusSource::new(table.clone(), POLL_INTERVAL);
        let dropped = spawn_wait(&source, 1);
        let wait = spawn_wait(&source, 2);
        sleep(POLL_INTERVAL * 4).await;

        dropped.abort();
        table.set(2, QueryJobStatus::Succeeded);
        assert_eq!(join(wait).await?, QueryJobStatus::Succeeded);
        sleep(POLL_INTERVAL * 4).await;
        let num_fetches = table.fetches().len();
        sleep(POLL_INTERVAL * 10).await;
        assert_eq!(
            table.fetches().len(),
            num_fetches,
            "polling should stop once no job is waited on"
        );

        table.set(1, QueryJobStatus::Succeeded);
        assert_eq!(
            join(spawn_wait(&source, 1)).await?,
            QueryJobStatus::Succeeded
        );
        assert_eq!(table.fetches().last(), Some(&vec![1]));
        Ok(())
    }
}
