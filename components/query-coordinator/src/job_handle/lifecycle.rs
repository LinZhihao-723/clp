//! The lifecycle shared by every job in the query jobs table.

use std::sync::Arc;

use clp_rust_utils::job_config::QUERY_JOBS_TABLE_NAME;
use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::job_config::QueryJobStatus;
use const_format::formatcp;
use spider_core::task::TaskGraph;
use spider_core::types::id::JobId as SpiderJobId;
use spider_core::types::id::ResourceGroupId;
use spider_core::types::io::TaskGraphInput;
use sqlx::MySql;
use sqlx::Transaction;

use crate::Error;
use crate::job_handle::JobHandleContext;
use crate::spider_job_submitter::JobOutcome;
use crate::spider_job_submitter::SpiderJobSubmitter;

/// The lifecycle of one job in the query jobs table: its submission to Spider and every transition
/// of its status.
///
/// # Type Parameters
///
/// * `SubmitterType` - The type of the job submitter for Spider job submission.
pub(super) struct JobLifecycle<SubmitterType: SpiderJobSubmitter> {
    context: Arc<JobHandleContext>,
    job_id: QueryJobId,
    job_submitter: SubmitterType,
    resource_group_id: ResourceGroupId,
}

impl<SubmitterType: SpiderJobSubmitter> JobLifecycle<SubmitterType> {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A newly created [`JobLifecycle`] for the job identified by `job_id`.
    pub(super) const fn new(
        context: Arc<JobHandleContext>,
        job_id: QueryJobId,
        job_submitter: SubmitterType,
        resource_group_id: ResourceGroupId,
    ) -> Self {
        Self {
            context,
            job_id,
            job_submitter,
            resource_group_id,
        }
    }

    pub(super) fn context(&self) -> &JobHandleContext {
        &self.context
    }

    pub(super) const fn job_id(&self) -> QueryJobId {
        self.job_id
    }

    /// Submits a Spider job built from `graph` and `input`, and persists the job's running state
    /// with `num_tasks` tasks.
    ///
    /// If the running state can't be persisted, the Spider job is cancelled on a best-effort basis
    /// before the error is returned.
    ///
    /// # Returns
    ///
    /// The submitted Spider job ID on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`SpiderJobSubmitter::submit_job`]'s return values on failure.
    /// * Forwards [`Self::start`]'s return values on failure.
    pub(super) async fn submit(
        &self,
        graph: &TaskGraph,
        input: &TaskGraphInput,
        num_tasks: i32,
    ) -> Result<SpiderJobId, Error> {
        let spider_job_id = self
            .job_submitter
            .submit_job(self.resource_group_id, graph, input)
            .await?;

        tracing::info!(
            job_id = % self.job_id,
            spider_job_id = % spider_job_id,
            num_tasks,
            "Job submitted to Spider.",
        );

        if let Err(start_error) = self.start(spider_job_id, num_tasks).await {
            let _ = self
                .job_submitter
                .cancel_job(spider_job_id)
                .await
                .inspect_err(|e| {
                    tracing::warn!(
                        job_id = % self.job_id,
                        spider_job_id = % spider_job_id,
                        error = % e,
                        "Failed to cancel the Spider job after failing to start the job.",
                    );
                });
            return Err(start_error);
        }
        Ok(spider_job_id)
    }

    /// Resumes the job, which was already submitted to Spider.
    ///
    /// This method skips submission and waits for the Spider job identified by `spider_job_id` to
    /// reach a terminal state. On failure, it attempts to persist a terminal status for the job
    /// before the error is returned.
    ///
    /// NOTE: It's the caller's responsibility to ensure that the given Spider job ID is associated
    /// with the job.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::to_completion`]'s return values on failure.
    pub(super) async fn recover(&self, spider_job_id: SpiderJobId) -> Result<(), Error> {
        tracing::info!(
            job_id = % self.job_id,
            spider_job_id = % spider_job_id,
            "Recovering job.",
        );

        self.run_to_terminal(self.to_completion(spider_job_id))
            .await
    }

    /// Drives the job with `driver`, and finalizes the job with [`Self::finalize_on_error`] if
    /// `driver` fails.
    ///
    /// # Type Parameters
    ///
    /// * `DriverType` - The type of the future that drives the job to a terminal status.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards `driver`'s return values on failure.
    pub(super) async fn run_to_terminal<DriverType: Future<Output = Result<(), Error>>>(
        &self,
        driver: DriverType,
    ) -> Result<(), Error> {
        let result = driver.await;
        if let Err(error) = &result {
            self.finalize_on_error(error).await;
        }
        result
    }

    /// Waits for the associated Spider job to complete and finalizes the job.
    ///
    /// This method monitors the specified Spider job until it reaches a terminal state, then
    /// updates the job according to the Spider job's result.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`SpiderJobSubmitter::run_job_to_completion`]'s return values on failure.
    /// * Forwards [`Self::terminate`]'s return values on failure.
    pub(super) async fn to_completion(&self, spider_job_id: SpiderJobId) -> Result<(), Error> {
        let outcome = self
            .job_submitter
            .run_job_to_completion(spider_job_id, self.context.spider_option.poll_interval)
            .await?;

        tracing::info!(
            job_id = % self.job_id,
            spider_job_id = % spider_job_id,
            outcome = ? outcome,
            "Job reached a terminal Spider state.",
        );

        self.terminate(outcome).await
    }

    /// Terminates the job with the given outcome in the CLP database.
    ///
    /// This method runs in a transaction that locks the job's row, and persists the status that
    /// [`Self::resolve_terminal_status`] resolves from the locked status.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`sqlx::Pool::begin`]'s return values on failure.
    /// * Forwards [`Self::lock_and_get_status`]'s return values on failure.
    /// * Forwards [`Self::resolve_terminal_status`]'s return values on failure.
    /// * Forwards [`sqlx::query::Query::execute`]'s return values on failure.
    /// * Forwards [`sqlx::Transaction::commit`]'s return values on failure.
    pub(super) async fn terminate(&self, outcome: JobOutcome) -> Result<(), Error> {
        const UPDATE_QUERY: &str = formatcp!(
            "UPDATE `{QUERY_JOBS_TABLE_NAME}` SET `status` = ?, `status_msg` = ? WHERE `id` = ?"
        );

        let mut tx = self.context.db_pool.begin().await?;
        let current_status = self.lock_and_get_status(&mut tx).await?;
        let (status_to_persist, status_message) =
            self.resolve_terminal_status(outcome, current_status)?;

        sqlx::query(UPDATE_QUERY)
            .bind(status_to_persist)
            .bind(status_message)
            .bind(self.job_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        Ok(())
    }

    /// Finalizes the job after the handle failed to drive it to completion.
    ///
    /// This method logs `error` as a job-handle failure and then attempts to terminate the job with
    /// the outcome implied by `error`, as resolved by [`Error::job_outcome`].
    ///
    /// A failed attempt is logged and otherwise ignored, so that it can't re-enter this path.
    async fn finalize_on_error(&self, error: &Error) {
        tracing::error!(
            job_id = % self.job_id,
            error = % error,
            "Job failed.",
        );

        let Some(outcome) = error.job_outcome() else {
            return;
        };

        let Err(terminate_error) = self.terminate(outcome).await else {
            return;
        };
        match terminate_error {
            Error::InvalidJobStatusTransition { from, .. } if from.is_terminal() => {
                tracing::warn!(
                    job_id = % self.job_id,
                    from = ? from,
                    "Job already reached a terminal status; skipping the status update.",
                );
            }
            terminate_error => {
                tracing::error!(
                    job_id = % self.job_id,
                    error = % terminate_error,
                    "Failed to update job status on a job failure.",
                );
            }
        }
    }

    /// Resolves the status that terminating the job with `outcome` persists, given the job's
    /// `current_status`:
    ///
    /// * [`JobOutcome::Succeeded`] is permitted from [`QueryJobStatus::Pending`] and
    ///   [`QueryJobStatus::Running`]. From [`QueryJobStatus::CancelRequested`] and
    ///   [`QueryJobStatus::Cancelling`], the job is terminated as [`QueryJobStatus::Cancelled`]
    ///   instead, since a cancellation wins over a success.
    /// * [`JobOutcome::Cancelled`] is permitted only from [`QueryJobStatus::CancelRequested`] and
    ///   [`QueryJobStatus::Cancelling`]. From [`QueryJobStatus::Running`], the job is terminated as
    ///   [`QueryJobStatus::Failed`] instead.
    /// * [`JobOutcome::Failed`] is permitted from any non-terminal status, since a failure wins
    ///   over both a success and a cancellation.
    ///
    /// # Returns
    ///
    /// A tuple on success, containing:
    ///
    /// * The status to persist.
    /// * The status message to persist, if any.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Error::InvalidJobStatusTransition`] if `current_status` doesn't permit a transition to
    ///   `outcome`'s status.
    fn resolve_terminal_status(
        &self,
        outcome: JobOutcome,
        current_status: QueryJobStatus,
    ) -> Result<(QueryJobStatus, Option<String>), Error> {
        let to = QueryJobStatus::from(&outcome);
        match (outcome, current_status) {
            (JobOutcome::Succeeded, QueryJobStatus::Pending | QueryJobStatus::Running) => {
                Ok((to, None))
            }
            (
                JobOutcome::Cancelled,
                QueryJobStatus::CancelRequested | QueryJobStatus::Cancelling,
            ) => Ok((to, Some(CANCELLED_MESSAGE.to_owned()))),
            (
                JobOutcome::Succeeded,
                QueryJobStatus::CancelRequested | QueryJobStatus::Cancelling,
            ) => {
                tracing::info!(job_id = % self.job_id, "{CANCELLED_AFTER_COMPLETION_MESSAGE}");
                Ok((
                    QueryJobStatus::Cancelled,
                    Some(CANCELLED_AFTER_COMPLETION_MESSAGE.to_owned()),
                ))
            }
            (JobOutcome::Cancelled, QueryJobStatus::Running) => {
                tracing::error!(job_id = % self.job_id, "{CANCELLED_UNEXPECTED_MESSAGE}");
                Ok((
                    QueryJobStatus::Failed,
                    Some(CANCELLED_UNEXPECTED_MESSAGE.to_owned()),
                ))
            }
            (JobOutcome::Failed { error_message }, from) if !from.is_terminal() => {
                Ok((to, Some(error_message)))
            }
            (_, from) => Err(Error::InvalidJobStatusTransition { from, to }),
        }
    }

    /// Persists the Spider job ID and marks the job as running.
    ///
    /// This method associates the given Spider job ID with the job in the CLP database and updates
    /// the job status from [`QueryJobStatus::Pending`] to [`QueryJobStatus::Running`], in a
    /// transaction that locks the job's row.
    ///
    /// This method also ensures that the job has a valid `dispatch_time`, which the coordinator
    /// uses to mark jobs as dispatched. A coordinator restart may occur before the marker is
    /// persisted, leaving the Spider job running without a valid `dispatch_time`. Therefore, this
    /// method sets the field as part of row update if it has not already been set by the
    /// coordinator.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Error::JobCancelled`] if the job is in [`QueryJobStatus::CancelRequested`].
    /// * [`Error::InvalidJobStatusTransition`] if the job is in any other status than
    ///   [`QueryJobStatus::Pending`].
    /// * Forwards [`sqlx::Pool::begin`]'s return values on failure.
    /// * Forwards [`Self::lock_and_get_status`]'s return values on failure.
    /// * Forwards [`sqlx::query::Query::execute`]'s return values on failure.
    /// * Forwards [`sqlx::Transaction::commit`]'s return values on failure.
    async fn start(&self, spider_job_id: SpiderJobId, num_tasks: i32) -> Result<(), Error> {
        const UPDATE_QUERY: &str = formatcp!(
            "UPDATE `{QUERY_JOBS_TABLE_NAME}` SET `spider_id` = ?, `status` = ?, `num_tasks` = ?, \
             `start_time` = CURRENT_TIMESTAMP(3), `dispatch_time` = COALESCE(`dispatch_time`, \
             CURRENT_TIMESTAMP()) WHERE `id` = ?"
        );

        let mut tx = self.context.db_pool.begin().await?;
        match self.lock_and_get_status(&mut tx).await? {
            QueryJobStatus::Pending => {}
            QueryJobStatus::CancelRequested => return Err(Error::JobCancelled),
            from => {
                return Err(Error::InvalidJobStatusTransition {
                    from,
                    to: QueryJobStatus::Running,
                });
            }
        }

        sqlx::query(UPDATE_QUERY)
            .bind(spider_job_id.get())
            .bind(QueryJobStatus::Running)
            .bind(num_tasks)
            .bind(self.job_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        Ok(())
    }

    /// Reads the job's current status, locking its row until `tx` ends.
    ///
    /// # Returns
    ///
    /// The job's current status on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Error::JobMetadataCorrupted`] if the job's row no longer exists.
    /// * Forwards [`sqlx::query::QueryScalar::fetch_optional`]'s return values on failure.
    async fn lock_and_get_status(
        &self,
        tx: &mut Transaction<'_, MySql>,
    ) -> Result<QueryJobStatus, Error> {
        const SELECT_QUERY: &str =
            formatcp!("SELECT `status` FROM `{QUERY_JOBS_TABLE_NAME}` WHERE `id` = ? FOR UPDATE");

        sqlx::query_scalar::<_, QueryJobStatus>(SELECT_QUERY)
            .bind(self.job_id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Error::JobMetadataCorrupted(self.job_id))
    }
}

const CANCELLED_AFTER_COMPLETION_MESSAGE: &str =
    "The job completed, but it had already been requested to be cancelled.";
const CANCELLED_MESSAGE: &str = "The job was cancelled.";
const CANCELLED_UNEXPECTED_MESSAGE: &str =
    "The job was cancelled unexpectedly without a cancellation request.";

impl Error {
    /// # Returns
    ///
    /// The outcome to terminate a job with after its handle failed with this error, or `None` if
    /// nothing should be persisted, since the job's row is gone or the job already reached a
    /// terminal status through another path.
    fn job_outcome(&self) -> Option<JobOutcome> {
        match self {
            Self::JobMetadataCorrupted(_) => None,
            Self::InvalidJobStatusTransition { from, .. } if from.is_terminal() => None,
            Self::JobCancelled => Some(JobOutcome::Cancelled),
            error => Some(JobOutcome::Failed {
                error_message: format!("Job failed: {error}"),
            }),
        }
    }
}

impl From<&JobOutcome> for QueryJobStatus {
    fn from(outcome: &JobOutcome) -> Self {
        match outcome {
            JobOutcome::Succeeded => Self::Succeeded,
            JobOutcome::Failed { .. } => Self::Failed,
            JobOutcome::Cancelled => Self::Cancelled,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;
    use clp_rust_utils::clp_config::package::config::Database;
    use clp_rust_utils::job_config::QueryJobStatus;
    use spider_client::error::ClientError;
    use spider_core::task::TaskGraph;
    use spider_core::types::id::JobId as SpiderJobId;
    use spider_core::types::id::ResourceGroupId;
    use spider_core::types::io::TaskGraphInput;
    use spider_core::types::io::TaskGraphInputBuilder;
    use sqlx::mysql::MySqlPoolOptions;

    use super::CANCELLED_AFTER_COMPLETION_MESSAGE;
    use super::CANCELLED_MESSAGE;
    use super::CANCELLED_UNEXPECTED_MESSAGE;
    use super::JobLifecycle;
    use crate::Error;
    use crate::job_handle::ArchiveSelectionOptions;
    use crate::job_handle::JobHandleContext;
    use crate::job_handle::SpiderOption;
    use crate::spider_job_submitter::JobOutcome;
    use crate::spider_job_submitter::SpiderJobSubmitter;

    const SPIDER_JOB_ID: SpiderJobId = SpiderJobId::from(7);
    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

    #[tokio::test]
    async fn submit_cancels_the_spider_job_if_the_job_fails_to_start() -> anyhow::Result<()> {
        let submitter = MockSubmitter::new(true);
        let lifecycle = lifecycle(submitter.clone());

        let result = tokio::time::timeout(TEST_TIMEOUT, submit(&lifecycle)).await?;

        assert!(
            matches!(result, Err(Error::Sqlx(_))),
            "unexpected result: {result:?}"
        );
        assert_eq!(submitter.cancelled_spider_job_ids(), vec![SPIDER_JOB_ID]);
        Ok(())
    }

    #[tokio::test]
    async fn submit_neither_starts_nor_cancels_a_rejected_spider_job() -> anyhow::Result<()> {
        let submitter = MockSubmitter::new(false);
        let lifecycle = lifecycle(submitter.clone());

        let result = tokio::time::timeout(TEST_TIMEOUT, submit(&lifecycle)).await?;

        assert!(
            matches!(
                result,
                Err(Error::SpiderClient(ClientError::InvalidArgument(_)))
            ),
            "unexpected result: {result:?}"
        );
        assert_eq!(submitter.cancelled_spider_job_ids(), vec![]);
        Ok(())
    }

    #[tokio::test]
    async fn resolve_terminal_status_covers_every_outcome_and_status() {
        const ERROR_MESSAGE: &str = "the task failed";

        let lifecycle = lifecycle(MockSubmitter::new(true));
        let failed = JobOutcome::Failed {
            error_message: ERROR_MESSAGE.to_owned(),
        };
        for (outcome, current_status, expected) in [
            (
                JobOutcome::Succeeded,
                QueryJobStatus::Pending,
                Some((QueryJobStatus::Succeeded, None)),
            ),
            (
                JobOutcome::Succeeded,
                QueryJobStatus::Running,
                Some((QueryJobStatus::Succeeded, None)),
            ),
            (
                JobOutcome::Succeeded,
                QueryJobStatus::CancelRequested,
                Some((
                    QueryJobStatus::Cancelled,
                    Some(CANCELLED_AFTER_COMPLETION_MESSAGE),
                )),
            ),
            (
                JobOutcome::Succeeded,
                QueryJobStatus::Cancelling,
                Some((
                    QueryJobStatus::Cancelled,
                    Some(CANCELLED_AFTER_COMPLETION_MESSAGE),
                )),
            ),
            (JobOutcome::Succeeded, QueryJobStatus::Succeeded, None),
            (JobOutcome::Succeeded, QueryJobStatus::Failed, None),
            (JobOutcome::Succeeded, QueryJobStatus::Cancelled, None),
            (JobOutcome::Succeeded, QueryJobStatus::Killed, None),
            (JobOutcome::Cancelled, QueryJobStatus::Pending, None),
            (
                JobOutcome::Cancelled,
                QueryJobStatus::Running,
                Some((QueryJobStatus::Failed, Some(CANCELLED_UNEXPECTED_MESSAGE))),
            ),
            (
                JobOutcome::Cancelled,
                QueryJobStatus::CancelRequested,
                Some((QueryJobStatus::Cancelled, Some(CANCELLED_MESSAGE))),
            ),
            (
                JobOutcome::Cancelled,
                QueryJobStatus::Cancelling,
                Some((QueryJobStatus::Cancelled, Some(CANCELLED_MESSAGE))),
            ),
            (JobOutcome::Cancelled, QueryJobStatus::Succeeded, None),
            (JobOutcome::Cancelled, QueryJobStatus::Failed, None),
            (JobOutcome::Cancelled, QueryJobStatus::Cancelled, None),
            (JobOutcome::Cancelled, QueryJobStatus::Killed, None),
            (
                failed.clone(),
                QueryJobStatus::Pending,
                Some((QueryJobStatus::Failed, Some(ERROR_MESSAGE))),
            ),
            (
                failed.clone(),
                QueryJobStatus::Running,
                Some((QueryJobStatus::Failed, Some(ERROR_MESSAGE))),
            ),
            (
                failed.clone(),
                QueryJobStatus::CancelRequested,
                Some((QueryJobStatus::Failed, Some(ERROR_MESSAGE))),
            ),
            (
                failed.clone(),
                QueryJobStatus::Cancelling,
                Some((QueryJobStatus::Failed, Some(ERROR_MESSAGE))),
            ),
            (failed.clone(), QueryJobStatus::Succeeded, None),
            (failed.clone(), QueryJobStatus::Failed, None),
            (failed.clone(), QueryJobStatus::Cancelled, None),
            (failed, QueryJobStatus::Killed, None),
        ] {
            let to = QueryJobStatus::from(&outcome);
            let resolved = match lifecycle.resolve_terminal_status(outcome.clone(), current_status)
            {
                Ok(resolved) => Ok(resolved),
                Err(Error::InvalidJobStatusTransition { from, to }) => Err((from, to)),
                Err(error) => panic!("unexpected error: {error}"),
            };
            assert_eq!(
                resolved,
                expected
                    .map(|(status, message)| (status, message.map(str::to_owned)))
                    .ok_or((current_status, to)),
                "outcome {outcome:?}, current status {current_status:?}"
            );
        }
    }

    #[test]
    fn job_outcome_fails_the_job_unless_it_is_gone_cancelled_or_already_terminal() {
        for (error, expected) in [
            (Error::JobMetadataCorrupted(1), None),
            (
                Error::InvalidJobStatusTransition {
                    from: QueryJobStatus::Succeeded,
                    to: QueryJobStatus::Failed,
                },
                None,
            ),
            (
                Error::InvalidJobStatusTransition {
                    from: QueryJobStatus::Running,
                    to: QueryJobStatus::Running,
                },
                Some(JobOutcome::Failed {
                    error_message: "Job failed: invalid job status transition from Running to \
                                    Running"
                        .to_owned(),
                }),
            ),
            (Error::JobCancelled, Some(JobOutcome::Cancelled)),
            (
                Error::InvalidJobConfig("unknown dataset".to_owned()),
                Some(JobOutcome::Failed {
                    error_message: "Job failed: invalid job configuration: unknown dataset"
                        .to_owned(),
                }),
            ),
        ] {
            assert_eq!(error.job_outcome(), expected, "error {error:?}");
        }
    }

    /// A Spider job submitter that registers every job as [`SPIDER_JOB_ID`] unless it's configured
    /// to reject submissions, and that records the Spider jobs it cancels.
    #[derive(Clone)]
    struct MockSubmitter {
        accepts_submissions: bool,
        cancelled_spider_job_ids: Arc<Mutex<Vec<SpiderJobId>>>,
    }

    impl MockSubmitter {
        /// Factory function.
        ///
        /// # Returns
        ///
        /// A newly created [`MockSubmitter`] that accepts submissions if `accepts_submissions`.
        fn new(accepts_submissions: bool) -> Self {
            Self {
                accepts_submissions,
                cancelled_spider_job_ids: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// # Returns
        ///
        /// The Spider jobs cancelled so far, in order.
        ///
        /// # Panics
        ///
        /// Panics if the mutex is poisoned.
        fn cancelled_spider_job_ids(&self) -> Vec<SpiderJobId> {
            self.cancelled_spider_job_ids
                .lock()
                .expect("the mutex shouldn't be poisoned")
                .clone()
        }
    }

    #[async_trait]
    impl SpiderJobSubmitter for MockSubmitter {
        async fn submit_job(
            &self,
            _resource_group_id: ResourceGroupId,
            _graph: &TaskGraph,
            _input: &TaskGraphInput,
        ) -> Result<SpiderJobId, Error> {
            if self.accepts_submissions {
                Ok(SPIDER_JOB_ID)
            } else {
                Err(ClientError::InvalidArgument("rejected by the mock".to_owned()).into())
            }
        }

        async fn run_job_to_completion(
            &self,
            _spider_job_id: SpiderJobId,
            _poll_interval: Duration,
        ) -> Result<JobOutcome, Error> {
            Ok(JobOutcome::Succeeded)
        }

        async fn cancel_job(&self, spider_job_id: SpiderJobId) -> Result<(), Error> {
            self.cancelled_spider_job_ids
                .lock()
                .expect("the mutex shouldn't be poisoned")
                .push(spider_job_id);
            Ok(())
        }
    }

    /// # Returns
    ///
    /// A lifecycle of a job driven by `submitter`, whose database is unreachable so that any SQL
    /// statement fails promptly.
    ///
    /// # Panics
    ///
    /// Panics if the database URL is invalid.
    fn lifecycle(submitter: MockSubmitter) -> JobLifecycle<MockSubmitter> {
        let context = JobHandleContext {
            db_pool: MySqlPoolOptions::new()
                .acquire_timeout(Duration::from_millis(200))
                .connect_lazy("mysql://127.0.0.1:1/clp-db")
                .expect("the database URL should be valid"),
            db_config: Database::default(),
            archive_selection_options: ArchiveSelectionOptions {
                archive_retention_period_millisecs: None,
                max_datasets_per_query: None,
            },
            spider_option: SpiderOption {
                poll_interval: Duration::from_millis(1),
                query_task_max_retry: 0,
            },
        };
        JobLifecycle::new(Arc::new(context), 1, submitter, ResourceGroupId::from(1))
    }

    /// Submits an empty graph through `lifecycle`.
    ///
    /// # Returns
    ///
    /// The result of [`JobLifecycle::submit`].
    ///
    /// # Panics
    ///
    /// Panics if the empty graph can't be built.
    async fn submit(lifecycle: &JobLifecycle<MockSubmitter>) -> Result<SpiderJobId, Error> {
        let graph = TaskGraph::new(None, None).expect("an empty graph should be valid");
        let input = TaskGraphInputBuilder::new().build();
        lifecycle.submit(&graph, &input, 1).await
    }
}
