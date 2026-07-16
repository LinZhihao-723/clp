//! The compression-job-submission API trait for driving CLP S3 compression jobs on a Spider
//! (Huntsman) cluster.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use spider_core::types::id::{JobId, ResourceGroupId};

use crate::{
    error::Error,
    task_io::{ClpSCompressionOption, DbConfig, S3InputSource},
};

/// The terminal outcome of a compression job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompressionJobCompletion {
    /// Every compression task and the commit task completed successfully.
    Succeeded,

    /// The job reached a terminal failed state.
    Failed { error_message: String },

    /// The job was cancelled before reaching completion.
    Cancelled,
}

/// Drives CLP S3 compression jobs on a Spider (Huntsman) cluster.
#[async_trait]
pub trait S3CompressionJobSubmitter: Send + Sync {
    /// Builds the fan-out → commit task graph for `input_sources` and registers it with Spider,
    /// without starting it.
    ///
    /// # Parameters
    ///
    /// * `resource_group_id` - The Spider resource group to register the job under.
    /// * `clp_s_option` - `clp-s` tuning options shared by every task in the job.
    /// * `dataset` - The job's `CLP_S` dataset; effectively required for `CLP_S` + S3.
    /// * `db_config` - Metadata-DB connection shared by every task (for the archive indexer).
    /// * `input_sources` - One entry per compression task, already partitioned upstream.
    ///
    /// # Returns
    ///
    /// The [`JobId`] assigned to the registered job on success, to be persisted as
    /// `compression_jobs.spider_job_id`.
    ///
    /// # Errors
    ///
    /// Implementations must document their error conditions.
    async fn submit_s3_compression_job(
        &self,
        resource_group_id: ResourceGroupId,
        clp_s_option: ClpSCompressionOption,
        dataset: Option<String>,
        db_config: DbConfig,
        input_sources: Vec<S3InputSource>,
    ) -> Result<JobId, Error>;

    /// Idempotently starts the job identified by `spider_job_id` (only if it hasn't already been
    /// started) and waits until it reaches a terminal state.
    ///
    /// Safe to call regardless of whether the job is not-yet-started, already running, or already
    /// terminal.
    ///
    /// # Parameters
    ///
    /// * `spider_job_id` - The job to start (if needed) and wait on.
    ///
    /// # Returns
    ///
    /// The job's terminal outcome on success.
    ///
    /// # Errors
    ///
    /// Implementations must document their error conditions.
    async fn run_s3_compression_job_to_completion(
        &self,
        spider_job_id: JobId,
    ) -> Result<CompressionJobCompletion, Error>;
}
