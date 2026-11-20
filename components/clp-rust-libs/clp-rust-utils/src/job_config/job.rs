//! The config of a job in the query jobs table.

use serde::Deserialize;
use serde::Serialize;

use crate::job_config::ExtractJobConfig;
use crate::job_config::QueryJobConfig;

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

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::num::NonZeroU64;

    use non_empty_string::NonEmptyString;

    use super::JobConfig;
    use crate::dataset::DatasetName;
    use crate::job_config::ExtractJobConfig;
    use crate::job_config::QueryJobConfig;
    use crate::job_config::QueryOutput;
    use crate::job_config::ResultsCacheOutput;
    use crate::job_config::SearchQuery;
    use crate::job_config::TimeRange;
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
            error
                .to_string()
                .contains("invalid value: integer `0`, expected a nonzero u64"),
            "unexpected error: {error}"
        );
        Ok(())
    }
}
