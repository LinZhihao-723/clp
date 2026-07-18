use std::collections::HashMap;

use clp_rust_utils::{
    job_config::{ClpIoConfig, CompressionJobId, CompressionJobStatus, InputConfig},
    serde::BrotliMsgpack,
};
use const_format::formatcp;
use sqlx::MySqlPool;

const COMPRESSION_JOB_TABLE_NAME: &str = "compression_jobs";

#[derive(Debug, sqlx::FromRow)]
struct CompressionJob {
    id: CompressionJobId,
    #[sqlx(rename = "clp_config")]
    clp_io_config: Vec<u8>,
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
            let clp_io_config: ClpIoConfig = BrotliMsgpack::deserialize(&job_row.clp_io_config)?;
            let input_type = match &clp_io_config.input {
                InputConfig::S3InputConfig { .. } => "s3",
                InputConfig::S3ObjectMetadataInputConfig { .. } => "s3_object_metadata",
            };
            tracing::info!(
                "Deserialized compression job {}: input_type={input_type}, output={:#?}",
                job_row.id,
                clp_io_config.output
            );
            self.schedule_job(job_row.id, clp_io_config).await?;
        }

        Ok(())
    }

    async fn schedule_job(
        &mut self,
        _job_id: CompressionJobId,
        _clp_io_config: ClpIoConfig,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    pub async fn poll_running_jobs(&mut self) -> bool {
        !self.scheduled_jobs.is_empty()
    }
}
