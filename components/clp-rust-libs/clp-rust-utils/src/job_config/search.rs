use std::num::NonZeroU16;
use std::num::NonZeroU32;

use non_empty_string::NonEmptyString;
use num_enum::IntoPrimitive;
use num_enum::TryFromPrimitive;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use strum::EnumString;
use utoipa::ToSchema;

use crate::dataset::NonEmptyDatasetNames;
use crate::job_config::ExtractJobConfig;
use crate::types::PositiveI64;
use crate::types::SessionToken;

pub const QUERY_JOBS_TABLE_NAME: &str = "query_jobs";

pub const QUERY_TASKS_TABLE_NAME: &str = "query_tasks";

pub type QueryJobId = i32;

/// A job's config, as stored in the query jobs table.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "type",
    content = "config",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum JobConfig {
    Query(QueryJobConfig),
    Extract(ExtractJobConfig),
}

/// The config of a job that runs a search query.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryJobConfig {
    pub query: SearchQuery,
    pub output: QueryOutput,
}

/// The search a query job runs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchQuery {
    /// The datasets to search within.
    #[schema(value_type = Vec<String>, min_items = 1)]
    pub datasets: NonEmptyDatasetNames,

    /// The search query as a KQL string.
    #[schema(value_type = String, min_length = 1)]
    pub query_string: NonEmptyString,

    /// The range of timestamps to search within. Omit or set to `null` for no bounds.
    #[serde(default, deserialize_with = "deserialize_nullable_time_range")]
    // Inlined because the API server's `WebUI` OpenAPI document has another schema named
    // `TimeRange`.
    #[schema(inline, nullable)]
    pub time_range: TimeRange,

    /// Whether the string match should be case-insensitive.
    #[serde(default)]
    pub ignore_case: bool,
}

/// An inclusive range of timestamps in Unix epoch milliseconds, where a missing bound leaves that
/// side unbounded. The begin timestamp never exceeds the end timestamp.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(try_from = "UncheckedTimeRange")]
pub struct TimeRange {
    begin_timestamp_millisecs: Option<i64>,
    end_timestamp_millisecs: Option<i64>,
}

impl TimeRange {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A time range bounded by the given inclusive timestamps on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ReversedTimeRangeError`] if both bounds are set and `begin_timestamp_millisecs` exceeds
    ///   `end_timestamp_millisecs`.
    pub const fn new(
        begin_timestamp_millisecs: Option<i64>,
        end_timestamp_millisecs: Option<i64>,
    ) -> Result<Self, ReversedTimeRangeError> {
        if let (Some(begin), Some(end)) = (begin_timestamp_millisecs, end_timestamp_millisecs)
            && begin > end
        {
            return Err(ReversedTimeRangeError {
                begin_timestamp_millisecs: begin,
                end_timestamp_millisecs: end,
            });
        }
        Ok(Self {
            begin_timestamp_millisecs,
            end_timestamp_millisecs,
        })
    }

    #[must_use]
    pub const fn begin_timestamp_millisecs(&self) -> Option<i64> {
        self.begin_timestamp_millisecs
    }

    #[must_use]
    pub const fn end_timestamp_millisecs(&self) -> Option<i64> {
        self.end_timestamp_millisecs
    }
}

impl TryFrom<UncheckedTimeRange> for TimeRange {
    type Error = ReversedTimeRangeError;

    fn try_from(value: UncheckedTimeRange) -> Result<Self, Self::Error> {
        Self::new(
            value.begin_timestamp_millisecs,
            value.end_timestamp_millisecs,
        )
    }
}

/// An error returned when a time range's begin timestamp exceeds its end timestamp.
#[derive(Debug, Eq, PartialEq, thiserror::Error)]
#[error(
    "begin timestamp {begin_timestamp_millisecs} is greater than end timestamp \
     {end_timestamp_millisecs}"
)]
pub struct ReversedTimeRangeError {
    begin_timestamp_millisecs: i64,
    end_timestamp_millisecs: i64,
}

/// The destination of a query job's results.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "type", rename_all = "snake_case")]
pub enum QueryOutput {
    ResultsCache(ResultsCacheOutput),
    Network(NetworkOutput),
    File {},
}

impl QueryOutput {
    /// # Returns
    ///
    /// Whether the output includes a timeline.
    #[must_use]
    pub const fn has_timeline(&self) -> bool {
        matches!(
            self,
            Self::ResultsCache(ResultsCacheOutput {
                aggregation: Some(Aggregation::CountByTime { .. }),
                ..
            })
        )
    }
}

/// The results cache that a query job's results are buffered in.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResultsCacheOutput {
    /// The maximum number of search results to keep. Omit for no limit.
    #[serde(default)]
    pub max_num_results: Option<NonZeroU32>,

    /// The aggregation to compute over all matching results, regardless of `max_num_results`.
    #[serde(default)]
    pub aggregation: Option<Aggregation>,
}

/// An aggregation over a query job's results.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields, tag = "type", rename_all = "snake_case")]
pub enum Aggregation {
    /// Counts the matching log events in each time bucket of the given width.
    CountByTime {
        /// The width of each time bucket, in milliseconds.
        #[schema(value_type = i64, minimum = 1)]
        bucket_size_millisecs: PositiveI64,
    },
}

/// The TCP listener that a query job's results are streamed to.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkOutput {
    pub host: NonEmptyString,
    pub port: NonZeroU16,
    pub session_token: SessionToken,
}

/// The status of a query job.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    EnumString,
    Eq,
    IntoPrimitive,
    PartialEq,
    Serialize,
    ToSchema,
    TryFromPrimitive,
    sqlx::Type,
)]
#[repr(i32)]
#[strum(ascii_case_insensitive)]
pub enum QueryJobStatus {
    Pending = 0,
    Running = 1,
    Succeeded = 2,
    Failed = 3,
    CancelRequested = 4,
    Cancelling = 5,
    Cancelled = 6,
    Killed = 7,
}

impl QueryJobStatus {
    /// # Returns
    ///
    /// Whether the status is terminal, i.e., whether the job has finished and its status will no
    /// longer change.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Killed
        )
    }
}

/// The cancellability of a query job, derived from its status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Cancellability {
    Cancellable,
    CancellationRequested,
    NotCancellable,
}

impl Cancellability {
    /// # Returns
    ///
    /// The cancellability of a query job in the given status.
    #[must_use]
    pub const fn from_status(status: QueryJobStatus) -> Self {
        match status {
            QueryJobStatus::Pending | QueryJobStatus::Running => Self::Cancellable,
            QueryJobStatus::CancelRequested
            | QueryJobStatus::Cancelling
            | QueryJobStatus::Cancelled => Self::CancellationRequested,
            QueryJobStatus::Succeeded | QueryJobStatus::Failed | QueryJobStatus::Killed => {
                Self::NotCancellable
            }
        }
    }
}

/// The fields of a [`TimeRange`] before its invariant is checked.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedTimeRange {
    begin_timestamp_millisecs: Option<i64>,
    end_timestamp_millisecs: Option<i64>,
}

/// Deserializes a [`TimeRange`] that may be `null`.
///
/// # Type Parameters
///
/// * `DeserializerType` - The type of the deserializer to read the time range from.
///
/// # Returns
///
/// The deserialized time range on success, or [`TimeRange::default`] (unbounded) if the value is
/// `null`.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`Option::<TimeRange>::deserialize`]'s return values on failure.
fn deserialize_nullable_time_range<'de, DeserializerType: Deserializer<'de>>(
    deserializer: DeserializerType,
) -> Result<TimeRange, DeserializerType::Error> {
    Ok(Option::<TimeRange>::deserialize(deserializer)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::num::NonZeroU64;

    use non_empty_string::NonEmptyString;

    use super::Cancellability;
    use super::JobConfig;
    use super::QueryJobConfig;
    use super::QueryJobStatus;
    use super::QueryOutput;
    use super::ResultsCacheOutput;
    use super::ReversedTimeRangeError;
    use super::SearchQuery;
    use super::TimeRange;
    use crate::dataset::DatasetName;
    use crate::job_config::ExtractJobConfig;
    use crate::types::non_empty_string::ExpectedNonEmpty;

    /// An extract job's config, as encoded by `msgpack.packb` in Python.
    const PYTHON_EXTRACT_JOB_CONFIG: &[u8] = b"\x82\xa4type\xa7extract\xa6config\x83\xa7dataset\
        \xa7default\xaaarchive_id\xd9$018e90e5-8b2a-4a61-a2fc-cac799936caf\xb1target_chunk_size\
        \xce\x08\x00\x00\x00";

    #[test]
    fn job_configs_written_by_python_decode_to_their_kind() -> anyhow::Result<()> {
        const PYTHON_QUERY_JOB_CONFIG: &[u8] = b"\x82\xa4type\xa5query\xa6config\x82\xa5query\
            \x84\xa8datasets\x91\xa7default\xacquery_string\xa1*\xaatime_range\x82\
            \xb9begin_timestamp_millisecs\x00\xb7end_timestamp_millisecs\
            \xcf\x00\x00\x01\xa0\xf3g\xa7\xee\xabignore_case\xc3\xa6output\x82\xa4type\
            \xadresults_cache\xafmax_num_results\xcd\x03\xe8";

        assert_eq!(
            rmp_serde::from_slice::<JobConfig>(PYTHON_QUERY_JOB_CONFIG)?,
            JobConfig::Query(QueryJobConfig {
                query: SearchQuery {
                    datasets: vec![DatasetName::default()].try_into()?,
                    query_string: NonEmptyString::from_static_str("*"),
                    time_range: TimeRange::new(Some(0), Some(1_790_790_051_822))?,
                    ignore_case: true,
                },
                output: QueryOutput::ResultsCache(ResultsCacheOutput {
                    max_num_results: NonZeroU32::new(1000),
                    aggregation: None,
                }),
            })
        );
        assert_eq!(
            rmp_serde::from_slice::<JobConfig>(PYTHON_EXTRACT_JOB_CONFIG)?,
            JobConfig::Extract(ExtractJobConfig {
                dataset: DatasetName::default(),
                archive_id: "018e90e5-8b2a-4a61-a2fc-cac799936caf".parse()?,
                target_chunk_size: NonZeroU64::new(134_217_728),
            })
        );
        Ok(())
    }

    #[test]
    fn extract_job_config_encodes_its_archive_id_as_a_string_like_python() -> anyhow::Result<()> {
        let job_config = JobConfig::Extract(ExtractJobConfig {
            dataset: DatasetName::default(),
            archive_id: "018e90e5-8b2a-4a61-a2fc-cac799936caf".parse()?,
            target_chunk_size: NonZeroU64::new(134_217_728),
        });

        assert_eq!(
            rmp_serde::to_vec_named(&job_config)?,
            PYTHON_EXTRACT_JOB_CONFIG
        );
        Ok(())
    }

    #[test]
    fn job_config_of_an_unknown_kind_is_rejected() -> anyhow::Result<()> {
        let encoded = rmp_serde::to_vec_named(&serde_json::json!({
            "type": "extract_ir",
            "config": {"orig_file_id": "file", "msg_ix": 0},
        }))?;

        let error = rmp_serde::from_slice::<JobConfig>(&encoded)
            .expect_err("a job config of an unknown kind should be rejected");

        assert!(
            error.to_string().contains("unknown variant `extract_ir`"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[test]
    fn extract_job_config_with_a_zero_target_chunk_size_is_rejected() -> anyhow::Result<()> {
        let encoded = rmp_serde::to_vec_named(&serde_json::json!({
            "type": "extract",
            "config": {
                "dataset": "default",
                "archive_id": "018e90e5-8b2a-4a61-a2fc-cac799936caf",
                "target_chunk_size": 0,
            },
        }))?;

        let error = rmp_serde::from_slice::<JobConfig>(&encoded)
            .expect_err("a zero target chunk size should be rejected");

        assert!(
            error.to_string().contains("nonzero"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[test]
    fn cancellability_from_status_covers_every_status() {
        for (status, expected) in [
            (QueryJobStatus::Pending, Cancellability::Cancellable),
            (QueryJobStatus::Running, Cancellability::Cancellable),
            (
                QueryJobStatus::CancelRequested,
                Cancellability::CancellationRequested,
            ),
            (
                QueryJobStatus::Cancelling,
                Cancellability::CancellationRequested,
            ),
            (
                QueryJobStatus::Cancelled,
                Cancellability::CancellationRequested,
            ),
            (QueryJobStatus::Succeeded, Cancellability::NotCancellable),
            (QueryJobStatus::Failed, Cancellability::NotCancellable),
            (QueryJobStatus::Killed, Cancellability::NotCancellable),
        ] {
            assert_eq!(
                Cancellability::from_status(status),
                expected,
                "status: {status:?}"
            );
        }
    }

    #[test]
    fn time_range_new_rejects_only_reversed_bounds() {
        for (begin, end) in [
            (Some(1), Some(2)),
            (Some(1), Some(1)),
            (Some(1), None),
            (None, Some(-1)),
            (None, None),
        ] {
            let time_range =
                TimeRange::new(begin, end).expect("non-reversed bounds should be accepted");
            assert_eq!(time_range.begin_timestamp_millisecs(), begin);
            assert_eq!(time_range.end_timestamp_millisecs(), end);
        }

        assert_eq!(
            TimeRange::new(Some(2), Some(1)),
            Err(ReversedTimeRangeError {
                begin_timestamp_millisecs: 2,
                end_timestamp_millisecs: 1,
            })
        );
    }

    #[test]
    fn null_time_range_deserializes_as_unbounded() {
        let config: QueryJobConfig = serde_json::from_value(serde_json::json!({
            "query": {"datasets": ["default"], "query_string": "*", "time_range": null},
            "output": {"type": "file"},
        }))
        .expect("a null time range should deserialize");

        assert_eq!(config.query.time_range, TimeRange::default());
    }

    #[test]
    fn reversed_time_range_in_a_search_query_is_rejected() {
        let error = serde_json::from_value::<QueryJobConfig>(serde_json::json!({
            "query": {
                "datasets": ["default"],
                "query_string": "*",
                "time_range": {"begin_timestamp_millisecs": 2, "end_timestamp_millisecs": 1},
            },
            "output": {"type": "file"},
        }))
        .expect_err("a reversed time range should be rejected");

        assert!(
            error.to_string().contains(
                &ReversedTimeRangeError {
                    begin_timestamp_millisecs: 2,
                    end_timestamp_millisecs: 1,
                }
                .to_string()
            ),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn file_output_with_an_unknown_field_is_rejected() {
        let error = serde_json::from_value::<QueryJobConfig>(serde_json::json!({
            "query": {"datasets": ["default"], "query_string": "*"},
            "output": {"type": "file", "unknown": 1},
        }))
        .expect_err("a file output with an unknown field should be rejected");

        assert!(
            error.to_string().contains("unknown field"),
            "unexpected error: {error}"
        );
    }
}
