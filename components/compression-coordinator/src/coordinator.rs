use std::collections::HashMap;

use clp_rust_utils::{
    job_config::{
        ClpIoConfig,
        CompressionJobId,
        CompressionJobStatus,
        InputConfig,
        S3ObjectMetadataInputConfig,
    },
    serde::{BrotliMsgpack, BrotliMsgpackBytes},
};
use const_format::formatcp;
use sqlx::MySqlPool;

use crate::partition::PathsToCompressBuffer;

const COMPRESSION_JOB_TABLE_NAME: &str = "compression_jobs";

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
            let clp_io_config: ClpIoConfig =
                match BrotliMsgpack::deserialize(&job_row.clp_io_config) {
                    Ok(config) => config,
                    Err(_) => {
                        const ERR_MSG: &str = "Failed to decompress job config. The config data \
                                               may have been corrupted or truncated.";
                        self.update_compression_job_metadata(
                            job_row.id,
                            CompressionJobStatus::Failed,
                            ERR_MSG,
                        )
                        .await?;
                        continue;
                    }
                };

            // Skips jobs that are not ingested from the log ingestor
            let s3_object_metadata_input_config = match &clp_io_config.input {
                InputConfig::S3InputConfig { .. } => continue,
                InputConfig::S3ObjectMetadataInputConfig { config } => config.clone(),
            };

            let mut paths_to_compress_buffer =
                PathsToCompressBuffer::new(job_row.id, clp_io_config, self.db_pool.clone());

            match Self::process_s3_object_metadata_input(
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
                    continue;
                }
            }
        }

        Ok(())
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
    async fn process_s3_object_metadata_input(
        _config: &S3ObjectMetadataInputConfig,
        _paths_to_compress_buffer: &mut PathsToCompressBuffer,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}
