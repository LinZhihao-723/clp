//! Archive metadata lookups and the archive-derived task settings shared by job handles.

use std::collections::HashSet;

use clp_rust_utils::dataset::DatasetName;
use clp_rust_utils::types::ArchiveId;
use spider_core::task::ExecutionPolicy;
use spider_core::task::TimeoutPolicy;

use crate::Error;
use crate::job_handle::JobHandleContext;

/// The timeout rates of the tasks that search an archive.
pub(super) const SEARCH_TIMEOUT_RATES: TimeoutRates = TimeoutRates {
    soft_timeout_millisecs_per_mib: 10,
    hard_timeout_millisecs_per_mib: 2 * 1000,
};

/// The timeout rates of the tasks that extract an archive's JSON stream.
pub(super) const EXTRACT_TIMEOUT_RATES: TimeoutRates = TimeoutRates {
    soft_timeout_millisecs_per_mib: 500,
    hard_timeout_millisecs_per_mib: 2 * 1000,
};

/// Metadata for an archive read by a job's tasks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ArchiveMetadata {
    /// The archive's ID.
    pub(super) id: ArchiveId,

    /// The archive's dataset.
    pub(super) dataset: DatasetName,

    /// The archive's uncompressed size in bytes.
    pub(super) uncompressed_size: u64,

    /// The archive's end timestamp in Unix epoch milliseconds.
    pub(super) end_timestamp: i64,
}

impl ArchiveMetadata {
    /// Checks whether the archive may be read by a job whose archives must end no earlier than
    /// `end_ts_lower_bound_millisecs`, as computed by
    /// [`JobHandleContext::archive_end_ts_lower_bound_millisecs`].
    ///
    /// An archive without an end timestamp is always within the bound, as is every archive when
    /// there's no bound.
    ///
    /// # Returns
    ///
    /// Whether the archive is within the bound.
    pub(super) fn is_within_retention(&self, end_ts_lower_bound_millisecs: Option<i64>) -> bool {
        end_ts_lower_bound_millisecs
            .is_none_or(|lower_bound| self.end_timestamp == 0 || self.end_timestamp >= lower_bound)
    }
}

/// The rates, per MiB of an archive's uncompressed size, at which a task's timeouts grow.
pub(super) struct TimeoutRates {
    /// An aggressive estimate of the task's execution time.
    soft_timeout_millisecs_per_mib: u64,

    /// A conservative deadline that assumes the entire archive is extracted.
    hard_timeout_millisecs_per_mib: u64,
}

impl TimeoutRates {
    /// Derives the execution policy of a task that reads an archive whose uncompressed size is
    /// `archive_uncompressed_size` bytes.
    ///
    /// Both timeouts grow with the archive's size at these rates, within the bounds that Spider
    /// accepts, and the soft timeout is always below the hard one.
    ///
    /// # Returns
    ///
    /// The task's execution policy.
    pub(super) fn task_execution_policy(
        &self,
        archive_uncompressed_size: u64,
        max_num_retry: u32,
    ) -> ExecutionPolicy {
        const BYTES_PER_MIB: u64 = 1024 * 1024;

        let hard_timeout_ms = (archive_uncompressed_size
            .saturating_mul(self.hard_timeout_millisecs_per_mib)
            / BYTES_PER_MIB)
            .clamp(MIN_HARD_TIMEOUT_MILLISECS, MAX_TIMEOUT_MILLISECS);
        let soft_timeout_ms = (archive_uncompressed_size
            .saturating_mul(self.soft_timeout_millisecs_per_mib)
            / BYTES_PER_MIB)
            .clamp(MIN_SOFT_TIMEOUT_MILLISECS, hard_timeout_ms - 1);

        ExecutionPolicy {
            max_num_retry,
            timeout_policy: TimeoutPolicy {
                soft_timeout_ms,
                hard_timeout_ms,
            },
            ..ExecutionPolicy::default()
        }
    }
}

/// Columns projected from an archives table.
///
/// [`ArchiveMetadata`] can't be decoded directly from a row since the dataset is encoded in the
/// archives table's name rather than stored in a column.
#[derive(sqlx::FromRow)]
pub(super) struct ArchiveRowProjection {
    id: ArchiveId,
    #[sqlx(try_from = "i64")]
    uncompressed_size: u64,
    end_timestamp: i64,
}

impl ArchiveRowProjection {
    /// # Returns
    ///
    /// The metadata of the projected archive, which belongs to `dataset`.
    pub(super) const fn into_metadata(self, dataset: DatasetName) -> ArchiveMetadata {
        ArchiveMetadata {
            id: self.id,
            dataset,
            uncompressed_size: self.uncompressed_size,
            end_timestamp: self.end_timestamp,
        }
    }
}

impl JobHandleContext {
    /// Computes the lower bound of the end timestamps of the archives that a job created at
    /// `job_creation_timestamp_millisecs` may read, given the archive retention period.
    ///
    /// # Returns
    ///
    /// The lower bound in Unix epoch milliseconds, or `None` if archives are retained forever.
    pub(super) fn archive_end_ts_lower_bound_millisecs(
        &self,
        job_creation_timestamp_millisecs: i64,
    ) -> Option<i64> {
        self.archive_selection_options
            .archive_retention_period_millisecs
            .map(|period| job_creation_timestamp_millisecs.saturating_sub_unsigned(period.get()))
    }

    /// Checks that every dataset in `datasets` exists in the metadata database.
    ///
    /// `datasets` is assumed to be non-empty, since an empty set would make the generated `IN`
    /// clause invalid SQL.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Error::InvalidJobConfig`] if any dataset doesn't exist.
    /// * Forwards [`sqlx::query::QueryScalar::fetch_one`]'s return values on failure.
    pub(super) async fn ensure_datasets_exist(
        &self,
        datasets: &HashSet<DatasetName>,
    ) -> Result<(), Error> {
        let datasets_table = self.db_config.datasets_table_name();
        let mut query_builder = sqlx::QueryBuilder::<sqlx::MySql>::new(format!(
            "SELECT COUNT(*) FROM `{datasets_table}` WHERE CAST(`name` AS BINARY) IN ("
        ));
        let mut separated_datasets = query_builder.separated(", ");
        for dataset in datasets {
            separated_datasets.push_bind(dataset);
        }
        query_builder.push(")");

        let num_existing_datasets: i64 = query_builder
            .build_query_scalar()
            .fetch_one(&self.db_pool)
            .await?;
        if usize::try_from(num_existing_datasets).ok() != Some(datasets.len()) {
            return Err(Error::InvalidJobConfig(
                "one or more requested datasets don't exist".to_owned(),
            ));
        }

        Ok(())
    }

    /// Fetches the metadata of the archive identified by `archive_id` in `dataset`.
    ///
    /// # Returns
    ///
    /// The archive's metadata, or `None` if the dataset has no such archive, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`sqlx::query::QueryAs::fetch_optional`]'s return values on failure.
    pub(super) async fn fetch_archive(
        &self,
        dataset: &DatasetName,
        archive_id: ArchiveId,
    ) -> Result<Option<ArchiveMetadata>, Error> {
        let archives_table = self.db_config.archives_table_name(dataset);
        Ok(sqlx::query_as::<_, ArchiveRowProjection>(&format!(
            "SELECT `id`, `uncompressed_size`, `end_timestamp` FROM `{archives_table}` WHERE `id` \
             = ?"
        ))
        .bind(archive_id)
        .fetch_optional(&self.db_pool)
        .await?
        .map(|row| row.into_metadata(dataset.clone())))
    }
}

const MIN_SOFT_TIMEOUT_MILLISECS: u64 = 1000;
const MIN_HARD_TIMEOUT_MILLISECS: u64 = 10_000;

/// The maximum timeout accepted by Spider, in milliseconds (24 hours).
const MAX_TIMEOUT_MILLISECS: u64 = 1000 * 60 * 60 * 24;

#[cfg(test)]
mod tests {
    use clp_rust_utils::dataset::DatasetName;

    use super::ArchiveMetadata;
    use super::EXTRACT_TIMEOUT_RATES;
    use super::MAX_TIMEOUT_MILLISECS;
    use super::MIN_HARD_TIMEOUT_MILLISECS;
    use super::MIN_SOFT_TIMEOUT_MILLISECS;
    use super::SEARCH_TIMEOUT_RATES;

    #[test]
    fn task_execution_policy_keeps_timeouts_within_bounds() {
        const MAX_NUM_RETRY: u32 = 3;
        const ARCHIVE_UNCOMPRESSED_SIZES: [u64; 6] = [
            0,
            1,
            1024 * 1024,
            1024 * 1024 * 1024,
            1024 * 1024 * 1024 * 1024,
            u64::MAX,
        ];

        for timeout_rates in [&SEARCH_TIMEOUT_RATES, &EXTRACT_TIMEOUT_RATES] {
            for archive_uncompressed_size in ARCHIVE_UNCOMPRESSED_SIZES {
                let policy =
                    timeout_rates.task_execution_policy(archive_uncompressed_size, MAX_NUM_RETRY);
                let soft_timeout_ms = policy.timeout_policy.soft_timeout_ms;
                let hard_timeout_ms = policy.timeout_policy.hard_timeout_ms;
                assert!(
                    (MIN_HARD_TIMEOUT_MILLISECS..=MAX_TIMEOUT_MILLISECS).contains(&hard_timeout_ms),
                    "hard timeout {hard_timeout_ms} out of bounds for {archive_uncompressed_size}"
                );
                assert!(
                    (MIN_SOFT_TIMEOUT_MILLISECS..hard_timeout_ms).contains(&soft_timeout_ms),
                    "soft timeout {soft_timeout_ms} not below hard timeout {hard_timeout_ms} for \
                     {archive_uncompressed_size}"
                );
                assert_eq!(policy.max_num_retry, MAX_NUM_RETRY);
            }
        }
    }

    #[test]
    fn retention_admits_archives_ending_at_or_after_the_bound_or_without_an_end()
    -> anyhow::Result<()> {
        const LOWER_BOUND: i64 = 1_000_000;

        for (end_timestamp, end_ts_lower_bound_millisecs, is_within_retention) in [
            (1, None, true),
            (0, Some(LOWER_BOUND), true),
            (LOWER_BOUND - 1, Some(LOWER_BOUND), false),
            (LOWER_BOUND, Some(LOWER_BOUND), true),
            (LOWER_BOUND + 1, Some(LOWER_BOUND), true),
        ] {
            let archive = ArchiveMetadata {
                id: "00000000-0000-0000-0000-000000000001".parse()?,
                dataset: DatasetName::default(),
                uncompressed_size: 0,
                end_timestamp,
            };
            assert_eq!(
                archive.is_within_retention(end_ts_lower_bound_millisecs),
                is_within_retention,
                "end timestamp {end_timestamp}, lower bound {end_ts_lower_bound_millisecs:?}"
            );
        }
        Ok(())
    }
}
