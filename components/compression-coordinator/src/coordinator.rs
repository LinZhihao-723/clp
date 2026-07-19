use std::collections::HashSet;

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
use spider_core::types::id::{JobId, ResourceGroupId};
use sqlx::MySqlPool;

use crate::{
    compression_job_submitter::{CompressionJobCompletion, S3CompressionJobSubmitter},
    partition::PathsToCompressBuffer,
    task_io::ClpSCompressionOption,
};

const COMPRESSION_JOB_TABLE_NAME: &str = "compression_jobs";
const INGESTED_S3_OBJECT_METADATA_TABLE_NAME: &str = "ingested_s3_object_metadata";

#[derive(Debug, sqlx::FromRow)]
struct CompressionJob {
    id: CompressionJobId,
    #[sqlx(rename = "clp_config")]
    clp_io_config: BrotliMsgpackBytes,
}

pub struct CompressionCoordinator<Submitter: S3CompressionJobSubmitter + Clone + 'static> {
    db_pool: MySqlPool,
    job_submitter: Submitter,
    resource_group_id: ResourceGroupId,
    last_polled_job_id: Option<CompressionJobId>,
}

impl<Submitter: S3CompressionJobSubmitter + Clone + 'static> CompressionCoordinator<Submitter> {
    pub fn new(
        db_pool: MySqlPool,
        job_submitter: Submitter,
        resource_group_id: ResourceGroupId,
    ) -> Self {
        Self {
            db_pool,
            job_submitter,
            resource_group_id,
            last_polled_job_id: None,
        }
    }

    async fn fetch_new_jobs(&mut self) -> anyhow::Result<Vec<CompressionJob>> {
        const FETCH_ALL_NEW_JOBS_QUERY: &str = formatcp!(
            "SELECT `id`, `clp_config` FROM `{table}` WHERE `status` = ? ORDER BY `id` ASC;",
            table = COMPRESSION_JOB_TABLE_NAME,
        );
        const FETCH_NEW_JOBS_AFTER_QUERY: &str = formatcp!(
            "SELECT `id`, `clp_config` FROM `{table}` WHERE `status` = ? AND `id` > ? ORDER BY \
             `id` ASC;",
            table = COMPRESSION_JOB_TABLE_NAME,
        );

        let query = match self.last_polled_job_id {
            Some(last_polled_job_id) => {
                sqlx::query_as::<_, CompressionJob>(FETCH_NEW_JOBS_AFTER_QUERY)
                    .bind(CompressionJobStatus::Pending)
                    .bind(last_polled_job_id)
            }
            None => sqlx::query_as::<_, CompressionJob>(FETCH_ALL_NEW_JOBS_QUERY)
                .bind(CompressionJobStatus::Pending),
        };

        let rows = query.fetch_all(&self.db_pool).await?;
        if let Some(last_row) = rows.last() {
            self.last_polled_job_id = Some(last_row.id);
        }

        Ok(rows)
    }

    pub async fn search_and_schedule_new_tasks(&mut self) -> anyhow::Result<()> {
        let jobs = self.fetch_new_jobs().await?;
        for job_row in jobs {
            let job_id = job_row.id;

            let db_pool = self.db_pool.clone();
            let job_submitter = self.job_submitter.clone();
            let resource_group_id = self.resource_group_id;
            tokio::spawn(async move {
                if let Err(err) =
                    Self::schedule_job(db_pool, job_submitter, resource_group_id, job_row).await
                {
                    tracing::error!(error = ?err, "Failed to schedule job {job_id}.");
                }
            });
        }

        Ok(())
    }

    async fn schedule_job(
        db_pool: MySqlPool,
        job_submitter: Submitter,
        resource_group_id: ResourceGroupId,
        job_row: CompressionJob,
    ) -> anyhow::Result<()> {
        let clp_io_config: ClpIoConfig = match BrotliMsgpack::deserialize(&job_row.clp_io_config) {
            Ok(config) => config,
            Err(_) => {
                const ERR_MSG: &str = "Failed to decompress job config. The config data may have \
                                       been corrupted or truncated.";
                Self::update_compression_job_metadata(
                    &db_pool,
                    job_row.id,
                    CompressionJobStatus::Failed,
                    ERR_MSG,
                )
                .await?;
                return Ok(());
            }
        };

        // Skips jobs that are not ingested from the log ingestor
        let s3_object_metadata_input_config = match &clp_io_config.input {
            InputConfig::S3InputConfig { .. } => {
                tracing::warn!(
                    "Skipping job {}: only jobs ingested from the log ingestor are supported.",
                    job_row.id
                );
                return Ok(());
            }
            InputConfig::S3ObjectMetadataInputConfig { config } => config.clone(),
        };

        let mut paths_to_compress_buffer = PathsToCompressBuffer::new(&clp_io_config);

        match Self::process_s3_object_metadata_input(
            &db_pool,
            &s3_object_metadata_input_config,
            &mut paths_to_compress_buffer,
        )
        .await
        {
            Ok(()) => {}
            Err(err) => {
                let status_msg = format!("Failed to process S3 object metadata input: {err}");
                Self::update_compression_job_metadata(
                    &db_pool,
                    job_row.id,
                    CompressionJobStatus::Failed,
                    &status_msg,
                )
                .await?;
                return Ok(());
            }
        }

        paths_to_compress_buffer.flush();
        let input_sources = paths_to_compress_buffer.into_tasks_input_sources();
        if input_sources.is_empty() {
            const ERR_MSG: &str = "No S3 objects were partitioned for compression.";
            Self::update_compression_job_metadata(
                &db_pool,
                job_row.id,
                CompressionJobStatus::Failed,
                ERR_MSG,
            )
            .await?;
            return Ok(());
        }

        let output_config = &clp_io_config.output;
        let clp_s_option = ClpSCompressionOption {
            target_encoded_size: output_config.target_segment_size
                + output_config.target_dictionaries_size,
            compression_level: i32::from(output_config.compression_level),
            timestamp_key: s3_object_metadata_input_config
                .timestamp_key
                .map(String::from),
        };
        let dataset = s3_object_metadata_input_config.dataset.map(String::from);

        let num_tasks = input_sources.len();
        let spider_job_id = match job_submitter
            .submit_s3_compression_job(resource_group_id, clp_s_option, dataset, input_sources)
            .await
        {
            Ok(spider_job_id) => spider_job_id,
            Err(err) => {
                let status_msg = format!("Failed to submit the compression job to Spider: {err}");
                Self::update_compression_job_metadata(
                    &db_pool,
                    job_row.id,
                    CompressionJobStatus::Failed,
                    &status_msg,
                )
                .await?;
                return Ok(());
            }
        };

        Self::mark_job_scheduled(&db_pool, job_row.id, spider_job_id, num_tasks).await?;

        match job_submitter
            .run_s3_compression_job_to_completion(spider_job_id)
            .await
        {
            Ok(CompressionJobCompletion::Succeeded) => {}
            Ok(CompressionJobCompletion::Failed { error_message }) => {
                let status_msg = format!("The Spider compression job failed: {error_message}");
                Self::update_compression_job_metadata(
                    &db_pool,
                    job_row.id,
                    CompressionJobStatus::Failed,
                    &status_msg,
                )
                .await?;
            }
            Ok(CompressionJobCompletion::Cancelled) => {
                const ERR_MSG: &str = "The Spider compression job was cancelled.";
                Self::update_compression_job_metadata(
                    &db_pool,
                    job_row.id,
                    CompressionJobStatus::Killed,
                    ERR_MSG,
                )
                .await?;
            }
            Err(err) => {
                let status_msg =
                    format!("Failed to wait for the Spider compression job to complete: {err}");
                Self::update_compression_job_metadata(
                    &db_pool,
                    job_row.id,
                    CompressionJobStatus::Failed,
                    &status_msg,
                )
                .await?;
            }
        }

        Ok(())
    }

    pub async fn poll_running_jobs(&mut self) -> bool {
        // TODO: Track in-flight Spider jobs and retrieve their results.
        false
    }

    async fn update_compression_job_metadata(
        db_pool: &MySqlPool,
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
            .execute(db_pool)
            .await?;

        Ok(())
    }

    async fn mark_job_scheduled(
        db_pool: &MySqlPool,
        job_id: CompressionJobId,
        spider_job_id: JobId,
        num_tasks: usize,
    ) -> anyhow::Result<()> {
        const QUERY: &str = formatcp!(
            "UPDATE `{table}` SET `spider_id` = ?, `status` = ?, `num_tasks` = ?, `start_time` = \
             CURRENT_TIMESTAMP(3), `update_time` = CURRENT_TIMESTAMP() WHERE `id` = ?;",
            table = COMPRESSION_JOB_TABLE_NAME,
        );

        let num_tasks = i32::try_from(num_tasks)
            .map_err(|_| anyhow::anyhow!("Number of tasks {num_tasks} exceeds `i32::MAX`."))?;

        sqlx::query(QUERY)
            .bind(spider_job_id.get())
            .bind(CompressionJobStatus::Running)
            .bind(num_tasks)
            .bind(job_id)
            .execute(db_pool)
            .await?;

        Ok(())
    }

    /// Fetches S3 object metadata rows from the `ingested_s3_object_metadata` table for the given
    /// `s3_object_metadata_ids` and `ingestion_job_id`, and adds the metadata to
    /// `paths_to_compress_buffer`.
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
        db_pool: &MySqlPool,
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
            .fetch_all(db_pool)
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
