use std::num::NonZeroU16;

use non_empty_string::NonEmptyString;
use num_enum::IntoPrimitive;
use num_enum::TryFromPrimitive;
use serde::Deserialize;
use serde::Serialize;
use strum::EnumString;
use utoipa::ToSchema;
use uuid::Uuid;

pub const QUERY_JOBS_TABLE_NAME: &str = "query_jobs";

pub type QueryJobId = i32;

/// The token that identifies a session to the search tasks streaming results to it.
pub type SessionToken = Uuid;

/// Mirror of `job_orchestration.scheduler.job_config.AggregationConfig`. Must be kept in sync.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct AggregationConfig {
    pub job_id: Option<i64>,
    pub reducer_host: Option<String>,
    pub reducer_port: Option<u16>,
    pub do_count_aggregation: Option<bool>,
    /// Milliseconds
    pub count_by_time_bucket_size: Option<i64>,
}

/// Mirror of `job_orchestration.scheduler.job_config.NetworkOutput`. Must be kept in sync.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NetworkOutput {
    pub host: NonEmptyString,
    pub port: NonZeroU16,
    #[serde(with = "uuid::serde::hyphenated")]
    pub session_token: SessionToken,
}

/// Mirror of `job_orchestration.scheduler.job_config.SearchJobConfig`. Must be kept in sync.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct SearchJobConfig {
    pub datasets: Option<Vec<String>>,
    pub query_string: String,
    pub max_num_results: u32,
    pub begin_timestamp: Option<i64>,
    pub end_timestamp: Option<i64>,
    pub ignore_case: bool,
    pub path_filter: Option<String>,
    pub network_output: Option<NetworkOutput>,
    pub aggregation_config: Option<AggregationConfig>,
    pub write_to_file: bool,
}

/// Mirror of `job_orchestration.scheduler.constants.QueryJobStatus`. Must be kept in sync.
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
    Cancelling = 4,
    Cancelled = 5,
    Killed = 6,
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

/// Mirror of `job_orchestration.scheduler.constants.QueryJobType`. Must be kept in sync.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    IntoPrimitive,
    PartialEq,
    Serialize,
    TryFromPrimitive,
    sqlx::Type,
)]
#[repr(i32)]
pub enum QueryJobType {
    SearchOrAggregation = 0,
    ExtractIr = 1,
    ExtractJson = 2,
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU16;

    use non_empty_string::NonEmptyString;
    use serde::Deserialize;
    use uuid::Uuid;

    use super::NetworkOutput;
    use super::SearchJobConfig;
    use crate::types::non_empty_string::ExpectedNonEmpty;

    const SESSION_TOKEN: &str = "6f1d3b52-8a4e-4c1b-9f6e-2d7a5c0b9e13";

    /// # Returns
    ///
    /// A search job config whose results are streamed to a network output.
    fn search_job_config_with_network_output() -> SearchJobConfig {
        SearchJobConfig {
            datasets: Some(vec!["default".to_owned()]),
            query_string: "*Transmitted*".to_owned(),
            network_output: Some(NetworkOutput {
                host: NonEmptyString::from_static_str("10.0.0.7"),
                port: NonZeroU16::new(40_123).expect("40,123 is nonzero"),
                session_token: Uuid::parse_str(SESSION_TOKEN).expect("valid session token UUID"),
            }),
            ..SearchJobConfig::default()
        }
    }

    #[test]
    fn search_job_config_with_network_output_round_trips_through_named_msgpack() {
        let expected = search_job_config_with_network_output();

        let serialized =
            rmp_serde::to_vec_named(&expected).expect("search job config should serialize");
        let actual: SearchJobConfig =
            rmp_serde::from_slice(&serialized).expect("search job config should deserialize");

        assert_eq!(expected, actual);
    }

    #[test]
    fn network_output_session_token_serializes_as_hyphenated_text() {
        #[derive(Deserialize)]
        struct TextualNetworkOutput {
            session_token: String,
        }

        #[derive(Deserialize)]
        struct TextualSearchJobConfig {
            network_output: Option<TextualNetworkOutput>,
        }

        let serialized = rmp_serde::to_vec_named(&search_job_config_with_network_output())
            .expect("search job config should serialize");
        let textual: TextualSearchJobConfig = rmp_serde::from_slice(&serialized)
            .expect("the session token should deserialize as a string");

        assert_eq!(
            textual
                .network_output
                .map(|network_output| network_output.session_token),
            Some(SESSION_TOKEN.to_owned())
        );
    }
}
