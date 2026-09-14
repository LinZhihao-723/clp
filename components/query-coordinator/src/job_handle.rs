//! Lifecycle management for one query job.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::num::NonZeroU32;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use clp_rust_utils::clp_config::package::config::Database;
use clp_rust_utils::clp_config::package::config::QueryCoordinator as CoordinatorConfig;
use clp_rust_utils::dataset::CLP_DEFAULT_DATASET_NAME;
use clp_rust_utils::job_config::ArchiveId;
use clp_rust_utils::job_config::QUERY_JOBS_TABLE_NAME;
use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::job_config::QueryJobStatus;
use clp_rust_utils::job_config::SearchJobConfig;
use clp_rust_utils::task_io::query::ClpSQueryOption;
use clp_rust_utils::task_io::query::OutputHandle;
use const_format::formatcp;
use mongodb::IndexModel;
use mongodb::bson::Document;
use mongodb::bson::doc;
use mongodb::options::IndexOptions;
use non_empty_string::NonEmptyString;
use spider_core::task::ExecutionPolicy;
use spider_core::task::TimeoutPolicy;
use spider_core::types::id::JobId as SpiderJobId;
use spider_core::types::id::ResourceGroupId;
use sqlx::MySqlPool;

use crate::Error;
use crate::query_job_submitter::ArchiveMetadata;
use crate::query_job_submitter::QueryJobOutcome;
use crate::query_job_submitter::QueryJobSubmitter;

/// Options for a query job running in Spider.
pub struct SpiderOption {
    pub initial_poll_backoff: Duration,
    pub max_poll_backoff: Duration,
}

/// Options for selecting the archives to search and the execution policy of their query tasks.
pub struct PlanningOption {
    archive_retention_period: Option<NonZeroU32>,
    max_datasets_per_query: Option<NonZeroUsize>,
    search_task_execution_policy: ExecutionPolicy,
}

impl PlanningOption {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A newly created [`PlanningOption`] on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Error::InvalidConfiguration`] if:
    ///   * The search task hard timeout is not greater than the soft timeout.
    ///   * The search task hard timeout exceeds Spider's maximum task timeout.
    pub fn new(
        coordinator_config: &CoordinatorConfig,
        archive_retention_period: Option<NonZeroU32>,
    ) -> Result<Self, Error> {
        const MILLISECS_PER_SEC: u64 = 1000;

        let soft_timeout_secs = coordinator_config.search_task_soft_timeout_secs.get();
        let hard_timeout_secs = coordinator_config.search_task_hard_timeout_secs.get();
        if hard_timeout_secs <= soft_timeout_secs {
            return Err(Error::InvalidConfiguration(format!(
                "`search_task_hard_timeout_secs` ({hard_timeout_secs}) must be greater than \
                 `search_task_soft_timeout_secs` ({soft_timeout_secs})"
            )));
        }
        if hard_timeout_secs > MAX_SEARCH_TASK_TIMEOUT_SECS {
            return Err(Error::InvalidConfiguration(format!(
                "`search_task_hard_timeout_secs` ({hard_timeout_secs}) must not exceed \
                 {MAX_SEARCH_TASK_TIMEOUT_SECS}"
            )));
        }

        Ok(Self {
            archive_retention_period,
            max_datasets_per_query: coordinator_config.max_datasets_per_query,
            search_task_execution_policy: ExecutionPolicy {
                max_num_retry: coordinator_config.search_task_max_retry,
                max_num_instances: coordinator_config.search_task_max_num_instances.get(),
                timeout_policy: TimeoutPolicy {
                    soft_timeout_ms: soft_timeout_secs * MILLISECS_PER_SEC,
                    hard_timeout_ms: hard_timeout_secs * MILLISECS_PER_SEC,
                },
            },
        })
    }
}

/// Coordinator-wide state shared by every query job handle.
pub struct JobHandleContext {
    pub db_pool: MySqlPool,
    pub db_config: Database,
    pub results_cache: mongodb::Database,
    pub output_handle: OutputHandle,
    pub planning_option: PlanningOption,
    pub spider_option: SpiderOption,
}

/// Drives one query job through archive selection, submission, and terminal persistence.
///
/// # Type Parameters
///
/// * `SubmitterType` - The type of the job submitter for Spider job submission.
pub struct QueryJobHandle<SubmitterType: QueryJobSubmitter> {
    context: Arc<JobHandleContext>,
    query_job_id: QueryJobId,
    job_submitter: SubmitterType,
    resource_group_id: ResourceGroupId,
    search_job_config: SearchJobConfig,
    clp_s_query_option: ClpSQueryOption,
}

impl<SubmitterType: QueryJobSubmitter> QueryJobHandle<SubmitterType> {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A newly created [`QueryJobHandle`] on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Error::InvalidQueryJobConfig`] if the query string is empty.
    pub fn new(
        context: Arc<JobHandleContext>,
        query_job_id: QueryJobId,
        job_submitter: SubmitterType,
        resource_group_id: ResourceGroupId,
        search_job_config: SearchJobConfig,
    ) -> Result<Self, Error> {
        let query_string = NonEmptyString::try_from(search_job_config.query_string.clone())
            .map_err(|_| {
                Error::InvalidQueryJobConfig("query string must not be empty".to_owned())
            })?;
        let clp_s_query_option = ClpSQueryOption {
            query_string,
            max_num_results: NonZeroU32::new(search_job_config.max_num_results),
            begin_timestamp_millisecs: search_job_config.begin_timestamp,
            end_timestamp_millisecs: search_job_config.end_timestamp,
            ignore_case: search_job_config.ignore_case,
        };

        Ok(Self {
            context,
            query_job_id,
            job_submitter,
            resource_group_id,
            search_job_config,
            clp_s_query_option,
        })
    }

    /// Starts the query job and drives it to a terminal state.
    ///
    /// On a failure before the job is durably running, this method makes a best-effort attempt to
    /// mark the CLP query job as failed before returning the original error. After the job is
    /// durably running, monitoring and terminal-persistence failures leave it running so recovery
    /// can reattach to Spider.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::start`]'s return values on failure.
    /// * Forwards [`Self::to_completion`]'s return values on failure.
    pub async fn run(self) -> Result<(), Error> {
        tracing::info!(query_job_id = % self.query_job_id, "Starting query job.");

        let spider_job_id = match self.start().await {
            Ok(Some(spider_job_id)) => spider_job_id,
            Ok(None) => return Ok(()),
            Err(error) => {
                if !matches!(error, Error::JobNotPending(_)) {
                    self.report_failure(&error).await;
                }
                return Err(error);
            }
        };
        self.to_completion(spider_job_id).await
    }

    /// Resumes a query job that was already submitted to Spider.
    ///
    /// The caller must ensure `spider_job_id` belongs to this CLP query job.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::to_completion`]'s return values on failure.
    pub async fn recover(self, spider_job_id: SpiderJobId) -> Result<(), Error> {
        tracing::info!(
            query_job_id = % self.query_job_id,
            spider_job_id = % spider_job_id,
            "Recovering query job.",
        );

        self.to_completion(spider_job_id).await
    }

    /// Starts the query job by submitting it to Spider, or marks it as succeeded if no archives are
    /// selected.
    ///
    /// # Returns
    ///
    /// On success, the submitted Spider job ID, or `None` if no archives are selected.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::prepare_task_inputs`]'s return values on failure.
    /// * Forwards [`Self::complete_without_archives`]'s return values on failure.
    /// * Forwards [`Self::submit`]'s return values on failure.
    async fn start(&self) -> Result<Option<SpiderJobId>, Error> {
        let archives_to_search = self.prepare_task_inputs().await?;
        if archives_to_search.is_empty() {
            self.complete_without_archives().await?;
            return Ok(None);
        }
        Ok(Some(self.submit(archives_to_search).await?))
    }

    /// Submits the query job to Spider and persists its running state.
    ///
    /// # Returns
    ///
    /// The submitted Spider job ID on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Error::TooManyQueryTasks`] if the number of query tasks exceeds `i32`'s range.
    /// * Forwards [`Self::prepare_results_collection`]'s return values on failure.
    /// * Forwards [`QueryJobSubmitter::submit_query_job`]'s return values on failure.
    /// * Forwards [`Self::persist_spider_job_id`]'s return values on failure.
    ///
    /// # Panics
    ///
    /// Panics if `archives_to_search` is empty.
    async fn submit(
        &self,
        archives_to_search: Vec<(ArchiveMetadata, ExecutionPolicy)>,
    ) -> Result<SpiderJobId, Error> {
        assert!(
            !archives_to_search.is_empty(),
            "a query job without archives to search must not be submitted"
        );
        let num_tasks = archives_to_search.len();
        let persisted_num_tasks =
            i32::try_from(num_tasks).map_err(|_| Error::TooManyQueryTasks(num_tasks))?;
        self.prepare_results_collection().await?;
        let spider_job_id = self
            .job_submitter
            .submit_query_job(
                self.query_job_id,
                self.resource_group_id,
                self.clp_s_query_option.clone(),
                self.context.output_handle.clone(),
                archives_to_search,
            )
            .await?;

        tracing::info!(
            query_job_id = % self.query_job_id,
            spider_job_id = % spider_job_id,
            num_tasks,
            "Query job submitted.",
        );

        self.persist_spider_job_id(spider_job_id, persisted_num_tasks)
            .await?;
        Ok(spider_job_id)
    }

    /// Prepares the archive inputs and execution policies for the query tasks.
    ///
    /// # Returns
    ///
    /// The archives to search, ordered from the latest end timestamp to the earliest, and their
    /// execution policies on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Error::InvalidQueryJobConfig`] if the begin timestamp is greater than the end timestamp.
    /// * Forwards [`Self::resolve_datasets`]'s return values on failure.
    /// * Forwards [`Self::fetch_archive_end_timestamp_lower_bound`]'s return values on failure.
    /// * Forwards [`Self::fetch_archives`]'s return values on failure.
    async fn prepare_task_inputs(&self) -> Result<Vec<(ArchiveMetadata, ExecutionPolicy)>, Error> {
        if let (Some(begin_timestamp), Some(end_timestamp)) = (
            self.search_job_config.begin_timestamp,
            self.search_job_config.end_timestamp,
        ) && begin_timestamp > end_timestamp
        {
            return Err(Error::InvalidQueryJobConfig(format!(
                "begin timestamp {begin_timestamp} is greater than end timestamp {end_timestamp}"
            )));
        }

        let datasets = self.resolve_datasets().await?;
        let archive_end_timestamp_lower_bound =
            self.fetch_archive_end_timestamp_lower_bound().await?;

        let mut selected_archives = Vec::new();
        for dataset in &datasets {
            selected_archives.extend(
                self.fetch_archives(dataset.as_ref(), archive_end_timestamp_lower_bound)
                    .await?,
            );
        }
        selected_archives.sort_by_key(|archive| Reverse(archive.end_timestamp));

        Ok(selected_archives
            .into_iter()
            .map(|archive| {
                (
                    archive.metadata,
                    self.context
                        .planning_option
                        .search_task_execution_policy
                        .clone(),
                )
            })
            .collect())
    }

    /// Resolves the datasets to search.
    ///
    /// # Returns
    ///
    /// The datasets to search on success, where `None` stands for the default dataset.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`sqlx::query::QueryScalar::fetch_all`]'s return values on failure.
    /// * Forwards [`validate_requested_datasets`]'s return values on failure.
    async fn resolve_datasets(&self) -> Result<Vec<Option<NonEmptyString>>, Error> {
        let query = format!(
            "SELECT `name` FROM `{}`",
            self.context.db_config.datasets_table_name()
        );
        let existing_datasets: HashSet<String> = sqlx::query_scalar(&query)
            .fetch_all(&self.context.db_pool)
            .await?
            .into_iter()
            .collect();

        let Some(requested_datasets) = &self.search_job_config.datasets else {
            return Ok(if existing_datasets.contains(CLP_DEFAULT_DATASET_NAME) {
                vec![None]
            } else {
                Vec::new()
            });
        };
        Ok(validate_requested_datasets(
            requested_datasets,
            &existing_datasets,
            self.context.planning_option.max_datasets_per_query,
        )?
        .into_iter()
        .map(Some)
        .collect())
    }

    /// Computes the earliest end timestamp of archives within the retention period, relative to the
    /// query job's creation time.
    ///
    /// # Returns
    ///
    /// On success, the earliest end timestamp in Unix epoch milliseconds, or `None` if archives
    /// have no retention period.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`sqlx::query::QueryScalar::fetch_one`]'s return values on failure.
    async fn fetch_archive_end_timestamp_lower_bound(&self) -> Result<Option<i64>, Error> {
        const MILLISECS_PER_MIN: i64 = 60 * 1000;
        const QUERY: &str = formatcp!(
            "SELECT CAST(UNIX_TIMESTAMP(`creation_time`) * 1000 AS SIGNED) FROM \
             `{QUERY_JOBS_TABLE_NAME}` WHERE `id` = ?"
        );

        let Some(archive_retention_period) = self.context.planning_option.archive_retention_period
        else {
            return Ok(None);
        };
        let creation_time_millisecs: i64 = sqlx::query_scalar(QUERY)
            .bind(self.query_job_id)
            .fetch_one(&self.context.db_pool)
            .await?;
        Ok(Some(
            creation_time_millisecs - i64::from(archive_retention_period.get()) * MILLISECS_PER_MIN,
        ))
    }

    /// Fetches the archives in `dataset` that overlap the query job's time range and are within the
    /// retention period.
    ///
    /// # Returns
    ///
    /// The selected archives on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`sqlx::query::QueryAs::fetch_all`]'s return values on failure.
    async fn fetch_archives(
        &self,
        dataset: Option<&NonEmptyString>,
        archive_end_timestamp_lower_bound: Option<i64>,
    ) -> Result<Vec<SelectedArchive>, Error> {
        let archives_table = self
            .context
            .db_config
            .archives_table_name(dataset.map(NonEmptyString::as_str));
        let mut query_builder = sqlx::QueryBuilder::<sqlx::MySql>::new(format!(
            "SELECT `id`, `size`, `end_timestamp` FROM `{archives_table}` WHERE TRUE"
        ));
        if let Some(end_timestamp) = self.search_job_config.end_timestamp {
            query_builder
                .push(" AND `begin_timestamp` <= ")
                .push_bind(end_timestamp);
        }
        if let Some(begin_timestamp) = self.search_job_config.begin_timestamp {
            query_builder
                .push(" AND `end_timestamp` >= ")
                .push_bind(begin_timestamp);
        }
        if let Some(lower_bound) = archive_end_timestamp_lower_bound {
            query_builder
                .push(" AND (`end_timestamp` >= ")
                .push_bind(lower_bound)
                .push(" OR `end_timestamp` = 0)");
        }

        Ok(query_builder
            .build_query_as::<ArchiveRowProjection>()
            .fetch_all(&self.context.db_pool)
            .await?
            .into_iter()
            .map(|row| SelectedArchive {
                metadata: ArchiveMetadata {
                    id: row.id,
                    dataset: dataset.cloned(),
                    size: row.size,
                },
                end_timestamp: row.end_timestamp,
            })
            .collect())
    }

    /// Creates the query job's results collection and its timestamp index in the results cache.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`mongodb::Collection::create_index`]'s return values on failure.
    async fn prepare_results_collection(&self) -> Result<(), Error> {
        const TIMESTAMP_INDEX_NAME: &str = "timestamp-descending";

        let index = IndexModel::builder()
            .keys(doc! { "timestamp": -1, "_id": 1 })
            .options(
                IndexOptions::builder()
                    .name(TIMESTAMP_INDEX_NAME.to_owned())
                    .build(),
            )
            .build();
        self.context
            .results_cache
            .collection::<Document>(&self.query_job_id.to_string())
            .create_index(index)
            .await?;
        Ok(())
    }

    /// Marks the query job as succeeded without submitting it to Spider.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Error::JobNotPending`] if the query job is no longer pending.
    /// * Forwards [`sqlx::query::Query::execute`]'s return values on failure.
    async fn complete_without_archives(&self) -> Result<(), Error> {
        const QUERY: &str = formatcp!(
            "UPDATE `{QUERY_JOBS_TABLE_NAME}` SET `status` = ?, `num_tasks` = 0, `start_time` = \
             CURRENT_TIMESTAMP(3), `duration` = 0 WHERE `id` = ? AND `status` = ?"
        );

        let result = sqlx::query(QUERY)
            .bind(QueryJobStatus::Succeeded)
            .bind(self.query_job_id)
            .bind(QueryJobStatus::Pending)
            .execute(&self.context.db_pool)
            .await?;
        if 1 != result.rows_affected() {
            return Err(Error::JobNotPending(self.query_job_id));
        }

        tracing::info!(
            query_job_id = % self.query_job_id,
            "No archives selected for the query job. Marked it as succeeded.",
        );
        Ok(())
    }

    /// Persists the Spider job ID and marks the query job as running.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Error::JobNotPending`] if the query job is no longer pending.
    /// * Forwards [`sqlx::query::Query::execute`]'s return values on failure.
    async fn persist_spider_job_id(
        &self,
        spider_job_id: SpiderJobId,
        num_tasks: i32,
    ) -> Result<(), Error> {
        let query = formatcp!(
            "UPDATE `{QUERY_JOBS_TABLE_NAME}` SET `spider_id` = ?, `status` = ?, `num_tasks` = ?, \
             `start_time` = CURRENT_TIMESTAMP(3) WHERE `id` = ? AND `status` = ?"
        );
        let result = sqlx::query(query)
            .bind(spider_job_id.get())
            .bind(QueryJobStatus::Running)
            .bind(num_tasks)
            .bind(self.query_job_id)
            .bind(QueryJobStatus::Pending)
            .execute(&self.context.db_pool)
            .await?;

        if 1 != result.rows_affected() {
            return Err(Error::JobNotPending(self.query_job_id));
        }
        Ok(())
    }

    /// Waits for the associated Spider job to complete and finalizes the query job.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::update_job_status`]'s return values on failure.
    /// * Forwards [`QueryJobSubmitter::run_query_job_to_completion`]'s return values on failure.
    async fn to_completion(&self, spider_job_id: SpiderJobId) -> Result<(), Error> {
        let outcome = self
            .job_submitter
            .run_query_job_to_completion(
                spider_job_id,
                self.context.spider_option.initial_poll_backoff,
                self.context.spider_option.max_poll_backoff,
            )
            .await?;

        tracing::info!(
            query_job_id = % self.query_job_id,
            spider_job_id = % spider_job_id,
            outcome = ? outcome,
            "Query job reached a terminal Spider state.",
        );

        let (status, status_message) = match outcome {
            QueryJobOutcome::Succeeded => (QueryJobStatus::Succeeded, None),
            QueryJobOutcome::Failed { error_message } => (
                QueryJobStatus::Failed,
                Some(format!("The Spider query job failed: {error_message}")),
            ),
            QueryJobOutcome::UnexpectedlyCancelled => (
                QueryJobStatus::Failed,
                Some("Spider unexpectedly cancelled the query job.".to_owned()),
            ),
        };
        self.update_job_status(status, status_message.as_deref(), QueryJobStatus::Running)
            .await?;
        Ok(())
    }

    /// Reports a query job orchestration failure.
    ///
    /// Logs the original error and makes a best-effort attempt to mark the query job as failed. If
    /// terminal-status persistence fails, the status-update error is logged and otherwise ignored.
    async fn report_failure(&self, error: &Error) {
        tracing::error!(
            query_job_id = % self.query_job_id,
            error = % error,
            "Query job orchestration failed.",
        );

        let _ = self
            .update_job_status(
                QueryJobStatus::Failed,
                Some(&format!("Query job orchestration failed: {error}")),
                QueryJobStatus::Pending,
            )
            .await
            .inspect_err(|status_error| {
                tracing::error!(
                    query_job_id = % self.query_job_id,
                    error = % status_error,
                    "Failed to persist the query job failure.",
                );
            });
    }

    /// Updates a query job only when it has the expected non-terminal status.
    /// Leaves the status message unchanged when `status_message` is `None`.
    /// A zero-row update is treated as success so an ineligible or missing job row is left
    /// unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`sqlx::query::Query::execute`]'s return values on failure.
    async fn update_job_status(
        &self,
        status: QueryJobStatus,
        status_message: Option<&str>,
        expected_status: QueryJobStatus,
    ) -> Result<(), sqlx::Error> {
        let query = formatcp!(
            "UPDATE `{QUERY_JOBS_TABLE_NAME}` SET `status` = ?, `status_msg` = COALESCE(LEFT(?, \
             512), `status_msg`), `duration` = CASE WHEN `start_time` IS NULL THEN 0 ELSE \
             TIMESTAMPDIFF(MICROSECOND, `start_time`, CURRENT_TIMESTAMP(3)) / 1000000.0 END WHERE \
             `id` = ? AND `status` = ?"
        );
        let query = sqlx::query(query)
            .bind(status)
            .bind(status_message)
            .bind(self.query_job_id)
            .bind(expected_status);
        query.execute(&self.context.db_pool).await?;
        Ok(())
    }
}

/// The maximum search task timeout, mirroring spider-core's private `MAX_TIMEOUT_MS` (24 hours).
const MAX_SEARCH_TASK_TIMEOUT_SECS: u64 = 24 * 60 * 60;

/// An archive selected for a query job, along with the end timestamp used to order the selection.
struct SelectedArchive {
    metadata: ArchiveMetadata,
    end_timestamp: i64,
}

/// A projection of the columns read from an archives table row.
#[derive(sqlx::FromRow)]
struct ArchiveRowProjection {
    #[sqlx(try_from = "String")]
    id: ArchiveId,
    #[sqlx(try_from = "i64")]
    size: u64,
    end_timestamp: i64,
}

/// Validates the datasets requested by a query job against the existing datasets.
///
/// # Returns
///
/// The requested datasets in their requested order, without duplicates, on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`Error::InvalidQueryJobConfig`] if:
///   * `requested_datasets` is empty.
///   * Any requested dataset name is empty.
///   * The number of distinct requested datasets exceeds `max_datasets_per_query`.
///   * Any requested dataset doesn't exist.
fn validate_requested_datasets(
    requested_datasets: &[String],
    existing_datasets: &HashSet<String>,
    max_datasets_per_query: Option<NonZeroUsize>,
) -> Result<Vec<NonEmptyString>, Error> {
    if requested_datasets.is_empty() {
        return Err(Error::InvalidQueryJobConfig(
            "the datasets list must not be empty".to_owned(),
        ));
    }

    let mut datasets: Vec<NonEmptyString> = Vec::new();
    for dataset in requested_datasets {
        let dataset = NonEmptyString::new(dataset.clone()).map_err(|_| {
            Error::InvalidQueryJobConfig("dataset names must not be empty".to_owned())
        })?;
        if !datasets.contains(&dataset) {
            datasets.push(dataset);
        }
    }
    if let Some(max_datasets_per_query) = max_datasets_per_query
        && datasets.len() > max_datasets_per_query.get()
    {
        return Err(Error::InvalidQueryJobConfig(format!(
            "the number of requested datasets ({}) exceeds `max_datasets_per_query` \
             ({max_datasets_per_query})",
            datasets.len()
        )));
    }

    let missing_datasets: Vec<&str> = datasets
        .iter()
        .map(NonEmptyString::as_str)
        .filter(|dataset| !existing_datasets.contains(*dataset))
        .collect();
    if !missing_datasets.is_empty() {
        return Err(Error::InvalidQueryJobConfig(format!(
            "datasets {missing_datasets:?} don't exist"
        )));
    }

    Ok(datasets)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::num::NonZeroU64;
    use std::num::NonZeroUsize;

    use clp_rust_utils::clp_config::package::config::QueryCoordinator as CoordinatorConfig;
    use clp_rust_utils::types::non_empty_string::ExpectedNonEmpty;
    use non_empty_string::NonEmptyString;

    use super::MAX_SEARCH_TASK_TIMEOUT_SECS;
    use super::PlanningOption;
    use super::validate_requested_datasets;
    use crate::Error;

    #[test]
    fn planning_option_maps_search_task_config_to_execution_policy() -> anyhow::Result<()> {
        use std::num::NonZeroU32;

        use spider_core::task::ExecutionPolicy;
        use spider_core::task::TimeoutPolicy;

        let coordinator_config = CoordinatorConfig {
            search_task_max_num_instances: NonZeroU32::new(3).expect("3 is nonzero"),
            search_task_max_retry: 4,
            search_task_soft_timeout_secs: NonZeroU64::new(5).expect("5 is nonzero"),
            search_task_hard_timeout_secs: NonZeroU64::new(6).expect("6 is nonzero"),
            max_datasets_per_query: NonZeroUsize::new(7),
            ..CoordinatorConfig::default()
        };
        let archive_retention_period = NonZeroU32::new(60);

        let planning_option = PlanningOption::new(&coordinator_config, archive_retention_period)?;

        assert_eq!(
            planning_option.archive_retention_period,
            archive_retention_period
        );
        assert_eq!(
            planning_option.max_datasets_per_query,
            coordinator_config.max_datasets_per_query
        );
        assert_eq!(
            planning_option.search_task_execution_policy,
            ExecutionPolicy {
                max_num_retry: 4,
                max_num_instances: 3,
                timeout_policy: TimeoutPolicy {
                    soft_timeout_ms: 5000,
                    hard_timeout_ms: 6000,
                },
            }
        );
        Ok(())
    }

    #[test]
    fn planning_option_rejects_hard_timeout_not_greater_than_soft_timeout() {
        let soft_timeout_secs = CoordinatorConfig::default().search_task_soft_timeout_secs;
        let equal_timeouts_config = CoordinatorConfig {
            search_task_hard_timeout_secs: soft_timeout_secs,
            ..CoordinatorConfig::default()
        };
        let smaller_hard_timeout_config = CoordinatorConfig {
            search_task_hard_timeout_secs: NonZeroU64::new(soft_timeout_secs.get() - 1)
                .expect("the default soft timeout is greater than one second"),
            ..CoordinatorConfig::default()
        };

        for coordinator_config in [equal_timeouts_config, smaller_hard_timeout_config] {
            assert!(matches!(
                PlanningOption::new(&coordinator_config, None),
                Err(Error::InvalidConfiguration(_))
            ));
        }
    }

    #[test]
    fn planning_option_accepts_hard_timeout_at_spider_maximum() -> anyhow::Result<()> {
        let coordinator_config = CoordinatorConfig {
            search_task_hard_timeout_secs: NonZeroU64::new(MAX_SEARCH_TASK_TIMEOUT_SECS)
                .expect("the maximum search task timeout is nonzero"),
            ..CoordinatorConfig::default()
        };

        let planning_option = PlanningOption::new(&coordinator_config, None)?;

        assert_eq!(
            planning_option
                .search_task_execution_policy
                .timeout_policy
                .hard_timeout_ms,
            MAX_SEARCH_TASK_TIMEOUT_SECS * 1000
        );
        Ok(())
    }

    #[test]
    fn planning_option_rejects_hard_timeout_exceeding_spider_maximum() {
        let hard_timeouts_secs = [
            NonZeroU64::new(MAX_SEARCH_TASK_TIMEOUT_SECS + 1)
                .expect("the maximum search task timeout plus one is nonzero"),
            NonZeroU64::MAX,
        ];

        for search_task_hard_timeout_secs in hard_timeouts_secs {
            let coordinator_config = CoordinatorConfig {
                search_task_hard_timeout_secs,
                ..CoordinatorConfig::default()
            };
            assert!(matches!(
                PlanningOption::new(&coordinator_config, None),
                Err(Error::InvalidConfiguration(_))
            ));
        }
    }

    #[test]
    fn validate_requested_datasets_deduplicates_in_requested_order() -> anyhow::Result<()> {
        let existing_datasets = HashSet::from(["a".to_owned(), "b".to_owned(), "c".to_owned()]);
        let requested_datasets = ["b".to_owned(), "a".to_owned(), "b".to_owned()];

        let datasets = validate_requested_datasets(&requested_datasets, &existing_datasets, None)?;

        assert_eq!(
            datasets,
            [
                NonEmptyString::from_static_str("b"),
                NonEmptyString::from_static_str("a"),
            ]
        );
        Ok(())
    }

    #[test]
    fn validate_requested_datasets_rejects_empty_list() {
        let existing_datasets = HashSet::from(["default".to_owned()]);

        assert!(matches!(
            validate_requested_datasets(&[], &existing_datasets, None),
            Err(Error::InvalidQueryJobConfig(_))
        ));
    }

    #[test]
    fn validate_requested_datasets_rejects_empty_dataset_name() {
        let existing_datasets = HashSet::from(["default".to_owned()]);
        let requested_datasets = ["default".to_owned(), String::new()];

        assert!(matches!(
            validate_requested_datasets(&requested_datasets, &existing_datasets, None),
            Err(Error::InvalidQueryJobConfig(_))
        ));
    }

    #[test]
    fn validate_requested_datasets_reports_every_missing_dataset() {
        let existing_datasets = HashSet::from(["default".to_owned()]);
        let requested_datasets = [
            "missing_a".to_owned(),
            "default".to_owned(),
            "missing_b".to_owned(),
        ];

        let Err(Error::InvalidQueryJobConfig(message)) =
            validate_requested_datasets(&requested_datasets, &existing_datasets, None)
        else {
            panic!("expected missing datasets to be rejected");
        };
        assert_eq!(
            message,
            r#"datasets ["missing_a", "missing_b"] don't exist"#
        );
    }

    #[test]
    fn validate_requested_datasets_enforces_max_datasets_per_query() -> anyhow::Result<()> {
        let existing_datasets = HashSet::from(["a".to_owned(), "b".to_owned(), "c".to_owned()]);
        let max_datasets_per_query = NonZeroUsize::new(2);
        let duplicated_datasets = ["a".to_owned(), "b".to_owned(), "a".to_owned()];
        let too_many_datasets = ["a".to_owned(), "b".to_owned(), "c".to_owned()];

        let datasets = validate_requested_datasets(
            &duplicated_datasets,
            &existing_datasets,
            max_datasets_per_query,
        )?;

        assert_eq!(
            datasets,
            [
                NonEmptyString::from_static_str("a"),
                NonEmptyString::from_static_str("b"),
            ]
        );
        assert!(matches!(
            validate_requested_datasets(
                &too_many_datasets,
                &existing_datasets,
                max_datasets_per_query
            ),
            Err(Error::InvalidQueryJobConfig(_))
        ));
        Ok(())
    }
}
