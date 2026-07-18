use std::collections::{HashMap, HashSet};

use clp_rust_utils::{
    job_config::{
        ClpIoConfig,
        CompressionJobId,
        CompressionJobStatus,
        InputConfig,
        S3ObjectMetadataInputConfig,
    },
    s3::{ObjectMetadata, S3ObjectMetadataId},
    serde::{BrotliMsgpack, BrotliMsgpackBytes},
};
use const_format::formatcp;
use non_empty_string::NonEmptyString;
use sqlx::MySqlPool;

use crate::partition::PathsToCompressBuffer;

const COMPRESSION_JOB_TABLE_NAME: &str = "compression_jobs";
const INGESTED_S3_OBJECT_METADATA_TABLE_NAME: &str = "ingested_s3_object_metadata";

#[derive(Debug, sqlx::FromRow)]
struct CompressionJob {
    id: CompressionJobId,
    #[sqlx(rename = "clp_config")]
    clp_io_config: BrotliMsgpackBytes,
}

pub struct CompressionCoordinator {
    db_pool: MySqlPool,
    scheduled_jobs: HashMap<CompressionJobId, ()>,
}

impl CompressionCoordinator {
    pub fn new(db_pool: MySqlPool) -> Self {
        Self {
            db_pool,
            scheduled_jobs: HashMap::new(),
        }
    }

    async fn fetch_new_jobs(&self) -> anyhow::Result<Vec<CompressionJob>> {
        const FETCH_NEW_JOBS_QUERY: &str = formatcp!(
            "SELECT `id`, `clp_config` FROM `{table}` WHERE `status` = ?;",
            table = COMPRESSION_JOB_TABLE_NAME,
        );

        let rows = sqlx::query_as::<_, CompressionJob>(FETCH_NEW_JOBS_QUERY)
            .bind(CompressionJobStatus::Pending)
            .fetch_all(&self.db_pool)
            .await?;

        Ok(rows)
    }

    pub async fn search_and_schedule_new_tasks(&mut self) -> anyhow::Result<()> {
        // TODO: poll existing dataset names

        let jobs = self.fetch_new_jobs().await?;
        for job_row in jobs {
            let job_id = job_row.id;

            match self.schedule_job(job_row).await {
                Ok(true) => continue,
                Ok(false) => {
                    tracing::debug!("Failed to schedule job {job_id}");
                    continue;
                }
                Err(err) => {
                    tracing::warn!(error = ?err, "Unexpected error while scheduling job {job_id}");
                    return Err(err);
                }
            }
        }

        Ok(())
    }

    async fn schedule_job(&self, job_row: CompressionJob) -> anyhow::Result<bool> {
        let clp_io_config: ClpIoConfig = match BrotliMsgpack::deserialize(&job_row.clp_io_config) {
            Ok(config) => config,
            Err(_) => {
                const ERR_MSG: &str = "Failed to decompress job config. The config data may have \
                                       been corrupted or truncated.";
                self.update_compression_job_metadata(
                    job_row.id,
                    CompressionJobStatus::Failed,
                    ERR_MSG,
                )
                .await?;
                return Ok(false);
            }
        };

        // Skips jobs that are not ingested from the log ingestor
        let s3_object_metadata_input_config = match &clp_io_config.input {
            InputConfig::S3InputConfig { .. } => return Ok(false),
            InputConfig::S3ObjectMetadataInputConfig { config } => config.clone(),
        };

        let mut paths_to_compress_buffer =
            PathsToCompressBuffer::new(job_row.id, clp_io_config, self.db_pool.clone());

        match self
            .process_s3_object_metadata_input(
                &s3_object_metadata_input_config,
                &mut paths_to_compress_buffer,
            )
            .await
        {
            Ok(()) => {}
            Err(err) => {
                let status_msg = format!("Failed to process S3 object metadata input: {err}");
                self.update_compression_job_metadata(
                    job_row.id,
                    CompressionJobStatus::Failed,
                    &status_msg,
                )
                .await?;
                return Ok(false);
            }
        }

        Ok(true)
    }

    pub async fn poll_running_jobs(&mut self) -> bool {
        !self.scheduled_jobs.is_empty()
    }

    async fn update_compression_job_metadata(
        &self,
        job_id: CompressionJobId,
        status: CompressionJobStatus,
        status_msg: &str,
    ) -> anyhow::Result<()> {
        const QUERY: &str = formatcp!(
            "UPDATE `{table}` SET `status` = ?, `status_msg` = ?, `update_time` = \
             CURRENT_TIMESTAMP() WHERE `id` = ?;",
            table = COMPRESSION_JOB_TABLE_NAME,
        );

        sqlx::query(QUERY)
            .bind(status)
            .bind(status_msg)
            .bind(job_id)
            .execute(&self.db_pool)
            .await?;

        Ok(())
    }

    /// Fetches S3 object metadata rows from the `ingested_s3_object_metadata` table for the given
    /// `s3_object_metadata_ids` and `ingestion_job_id`, and adds the metadata to
    /// `paths_to_compress_buffer`.
    ///
    /// # Parameters
    ///
    /// * `config`: Contains the ingestion job ID, requested S3 object metadata IDs, bucket, and
    ///   required key prefix.
    /// * `paths_to_compress_buffer`: The buffer to which validated object metadata is added.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if the database query fails to fetch the requested S3 object metadata.
    /// * [`anyhow::Error`] if no metadata rows are found for the requested metadata IDs and
    ///   ingestion job ID.
    /// * [`anyhow::Error`] if any requested metadata ID is missing from the query results.
    /// * [`anyhow::Error`] if a returned object key does not begin with the configured key prefix.
    async fn process_s3_object_metadata_input(
        &self,
        config: &S3ObjectMetadataInputConfig,
        paths_to_compress_buffer: &mut PathsToCompressBuffer,
    ) -> anyhow::Result<()> {
        let s3_object_metadata_ids = &config.s3_object_metadata_ids;
        let ingestion_job_id = config.ingestion_job_id;

        let mut query_builder = sqlx::QueryBuilder::<sqlx::MySql>::new(formatcp!(
            "SELECT `id`, `key`, `size` FROM `{table}` WHERE `id` IN (",
            table = INGESTED_S3_OBJECT_METADATA_TABLE_NAME,
        ));
        let mut separated_ids = query_builder.separated(", ");
        for id in s3_object_metadata_ids {
            separated_ids.push_bind(id);
        }
        query_builder
            .push(") AND `ingestion_job_id` = ")
            .push_bind(ingestion_job_id);

        let metadata_list = query_builder
            .build_query_as::<(S3ObjectMetadataId, String, u64)>()
            .fetch_all(&self.db_pool)
            .await?;
        if metadata_list.is_empty() {
            return Err(anyhow::anyhow!(
                "No rows found in {INGESTED_S3_OBJECT_METADATA_TABLE_NAME} for the given \
                 s3_object_metadata_ids and ingestion_job_id {}.",
                ingestion_job_id,
            ));
        }

        let returned_ids: HashSet<S3ObjectMetadataId> = metadata_list
            .iter()
            .map(|(metadata_id, ..)| *metadata_id)
            .collect();
        let requested_ids: HashSet<S3ObjectMetadataId> =
            s3_object_metadata_ids.iter().copied().collect();

        let mut missing_ids: Vec<S3ObjectMetadataId> =
            requested_ids.difference(&returned_ids).copied().collect();
        missing_ids.sort();

        if !missing_ids.is_empty() {
            return Err(anyhow::anyhow!(
                "Missing metadata rows in {INGESTED_S3_OBJECT_METADATA_TABLE_NAME} for \
                 ingestion_job_id {ingestion_job_id}: {missing_ids:?}."
            ));
        }

        for (_, key, size) in metadata_list {
            if !key.starts_with(config.s3_config.key_prefix.as_str()) {
                return Err(anyhow::anyhow!(
                    "Metadata key {key} does not start with the key prefix {}.",
                    config.s3_config.key_prefix,
                ));
            }

            let key = NonEmptyString::try_from(key).map_err(anyhow::Error::msg)?;
            let object_metadata = ObjectMetadata::new(config.s3_config.bucket.clone(), key, size);
            paths_to_compress_buffer.add_file(object_metadata);
        }

        Ok(())
    }
}
