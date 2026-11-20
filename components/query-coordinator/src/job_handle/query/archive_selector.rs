//! The selection of the archives that a query job searches.

use std::cmp::Reverse;
use std::collections::HashSet;

use clp_rust_utils::dataset::DatasetName;
use clp_rust_utils::job_config::QueryJobConfig;
use spider_core::task::ExecutionPolicy;

use crate::Error;
use crate::job_handle::JobHandleContext;
use crate::job_handle::archive::ArchiveMetadata;
use crate::job_handle::archive::ArchiveRowProjection;
use crate::job_handle::archive::SEARCH_TIMEOUT_RATES;

/// Selects the archives a query job should search, and derives each one's task execution policy.
///
/// # Lifetimes
///
/// * `'job_handle_lifetime` - The lifetime of the [`super::QueryJobHandle`] state borrowed by the
///   selector.
pub(super) struct ArchiveSelector<'job_handle_lifetime> {
    pub(super) context: &'job_handle_lifetime JobHandleContext,
    pub(super) query_job_config: &'job_handle_lifetime QueryJobConfig,
    pub(super) archive_end_ts_lower_bound_millisecs: Option<i64>,
}

impl ArchiveSelector<'_> {
    /// Prepares the task inputs for the query job.
    ///
    /// This method validates and deduplicates the job's requested datasets, then retrieves from the
    /// CLP database the metadata of the archives matching the query, ordered by descending archive
    /// end timestamp, and attaches an execution policy derived from each archive's uncompressed
    /// size to its task. Archives that end before the job's archive retention lower bound, if any,
    /// are excluded.
    ///
    /// # Returns
    ///
    /// A vector of tuples on success, where each tuple contains:
    ///
    /// * The [`ArchiveMetadata`] identifying the archive searched by a single query task.
    /// * The [`ExecutionPolicy`] for that task.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::deduplicate_and_validate_requested_datasets`]'s return values on failure.
    /// * Forwards [`JobHandleContext::ensure_datasets_exist`]'s return values on failure.
    /// * Forwards [`Self::fetch_archives`]'s return values on failure.
    pub(super) async fn select(&self) -> Result<Vec<(ArchiveMetadata, ExecutionPolicy)>, Error> {
        let datasets = self.deduplicate_and_validate_requested_datasets()?;
        self.context.ensure_datasets_exist(datasets.iter()).await?;

        let mut selected_archives = Vec::new();
        for dataset in &datasets {
            selected_archives.extend(self.fetch_archives(dataset).await?);
        }
        selected_archives.sort_by_key(|archive| Reverse(archive.end_timestamp));

        Ok(selected_archives
            .into_iter()
            .map(|archive| {
                let execution_policy = SEARCH_TIMEOUT_RATES.task_execution_policy(
                    archive.uncompressed_size,
                    self.context.spider_option.query_task_max_retry,
                );
                (archive, execution_policy)
            })
            .collect())
    }

    /// Validates and deduplicates the datasets requested by a query job.
    ///
    /// # Returns
    ///
    /// The distinct requested datasets on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Error::InvalidJobConfig`] if the number of distinct datasets exceeds
    ///   [`crate::job_handle::ArchiveSelectionOptions::max_datasets_per_query`].
    fn deduplicate_and_validate_requested_datasets(&self) -> Result<HashSet<DatasetName>, Error> {
        let datasets: HashSet<DatasetName> = self
            .query_job_config
            .query
            .datasets
            .as_slice()
            .iter()
            .cloned()
            .collect();

        if let Some(max_datasets_per_query) = self
            .context
            .archive_selection_options
            .max_datasets_per_query
            && datasets.len() > max_datasets_per_query.get()
        {
            return Err(Error::InvalidJobConfig(format!(
                "the number of requested datasets ({}) exceeds `max_datasets_per_query` \
                 ({max_datasets_per_query})",
                datasets.len()
            )));
        }

        Ok(datasets)
    }

    /// Selects archives from one dataset that overlap the query's time range and retention window.
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
    async fn fetch_archives(&self, dataset: &DatasetName) -> Result<Vec<ArchiveMetadata>, Error> {
        let archives_table = self.context.db_config.archives_table_name(dataset);
        let mut query_builder = sqlx::QueryBuilder::<sqlx::MySql>::new(format!(
            "SELECT `id`, `uncompressed_size`, `end_timestamp` FROM `{archives_table}` WHERE TRUE"
        ));
        let time_range = self.query_job_config.query.time_range;
        if let Some(end_timestamp) = time_range.end_timestamp_millisecs() {
            query_builder
                .push(" AND `begin_timestamp` <= ")
                .push_bind(end_timestamp);
        }
        if let Some(begin_timestamp) = time_range.begin_timestamp_millisecs() {
            query_builder
                .push(" AND `end_timestamp` >= ")
                .push_bind(begin_timestamp);
        }
        if let Some(lower_bound) = self.archive_end_ts_lower_bound_millisecs {
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
            .map(|row| row.into_metadata(dataset.clone()))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::num::NonZeroUsize;
    use std::time::Duration;

    use clp_rust_utils::clp_config::package::config::Database;
    use clp_rust_utils::dataset::DatasetName;
    use clp_rust_utils::dataset::NonEmptyDatasetNames;
    use clp_rust_utils::job_config::QueryJobConfig;
    use clp_rust_utils::job_config::QueryOutput;
    use clp_rust_utils::job_config::ResultsCacheOutput;
    use clp_rust_utils::job_config::SearchQuery;
    use clp_rust_utils::job_config::TimeRange;
    use clp_rust_utils::types::non_empty_string::ExpectedNonEmpty;
    use non_empty_string::NonEmptyString;
    use sqlx::mysql::MySqlPoolOptions;

    use super::ArchiveSelector;
    use crate::Error;
    use crate::job_handle::ArchiveSelectionOptions;
    use crate::job_handle::JobHandleContext;
    use crate::job_handle::SpiderOption;

    #[tokio::test]
    async fn duplicate_requested_datasets_count_once_toward_max_datasets_per_query() {
        let context = context(NonZeroUsize::new(1));
        let other_dataset: DatasetName = "other"
            .parse()
            .expect("name should be a valid dataset name");
        let select_datasets = |datasets| {
            let base_query_job_config = base_query_job_config();
            let query_job_config = QueryJobConfig {
                query: SearchQuery {
                    datasets: NonEmptyDatasetNames::try_from(datasets)
                        .expect("dataset names should be non-empty"),
                    ..base_query_job_config.query
                },
                ..base_query_job_config
            };
            ArchiveSelector {
                context: &context,
                query_job_config: &query_job_config,
                archive_end_ts_lower_bound_millisecs: None,
            }
            .deduplicate_and_validate_requested_datasets()
        };

        assert_eq!(
            select_datasets(vec![DatasetName::default(), DatasetName::default()])
                .expect("duplicate datasets should count once"),
            HashSet::from([DatasetName::default()])
        );
        let error = select_datasets(vec![DatasetName::default(), other_dataset])
            .expect_err("two distinct datasets should exceed the limit");
        assert!(
            matches!(
                &error,
                Error::InvalidJobConfig(message) if message.contains("max_datasets_per_query")
            ),
            "unexpected error: {error:?}"
        );
    }

    /// # Returns
    ///
    /// A job handle context whose database pool connects lazily, so tests that don't query the
    /// database need no database.
    ///
    /// # Panics
    ///
    /// Panics if the database URL is invalid.
    fn context(max_datasets_per_query: Option<NonZeroUsize>) -> JobHandleContext {
        JobHandleContext {
            db_pool: MySqlPoolOptions::new()
                .connect_lazy("mysql://localhost/clp-db")
                .expect("the database URL should be valid"),
            db_config: Database::default(),
            archive_selection_options: ArchiveSelectionOptions {
                archive_retention_period_millisecs: None,
                max_datasets_per_query,
            },
            spider_option: SpiderOption {
                poll_interval: Duration::from_secs(1),
                query_task_max_retry: 0,
            },
        }
    }

    /// # Returns
    ///
    /// A query job config that searches the default dataset and buffers its results in the
    /// results cache.
    ///
    /// # Panics
    ///
    /// Panics if the dataset names are empty.
    fn base_query_job_config() -> QueryJobConfig {
        QueryJobConfig {
            query: SearchQuery {
                datasets: NonEmptyDatasetNames::try_from(vec![DatasetName::default()])
                    .expect("dataset names should be non-empty"),
                query_string: NonEmptyString::from_static_str("*"),
                time_range: TimeRange::default(),
                ignore_case: false,
            },
            output: QueryOutput::ResultsCache(ResultsCacheOutput {
                max_num_results: None,
                aggregation: None,
            }),
        }
    }
}
