//! Handles for driving a single job of the query jobs table to completion.

mod archive;
mod extract;
mod lifecycle;
mod query;

use std::num::NonZeroU64;
use std::num::NonZeroUsize;
use std::time::Duration;

use clp_rust_utils::clp_config::package::config::Database;
pub use extract::ExtractJobHandle;
pub use query::QueryJobHandle;
use spider_core::types::id::JobId as SpiderJobId;
use sqlx::MySqlPool;

use crate::Error;

/// Drives one job of the query jobs table to a terminal status.
pub trait JobHandle: Send + 'static {
    /// Plans, submits, and drives a dispatched pending job to completion.
    ///
    /// On failure, the handle attempts to persist a terminal status for the job before the error is
    /// returned.
    ///
    /// # Errors
    ///
    /// Returns an error if the job can't be driven to a terminal status, e.g., because its config
    /// is invalid, a Spider request fails, or its row can't be updated.
    fn run(self) -> impl Future<Output = Result<(), Error>> + Send;

    /// Resumes a job whose Spider job was submitted by a previous coordinator instance, by waiting
    /// for the Spider job identified by `spider_job_id` to reach a terminal state.
    ///
    /// On failure, the handle attempts to persist a terminal status for the job before the error is
    /// returned.
    ///
    /// NOTE: It's the caller's responsibility to ensure that the given Spider job ID is associated
    /// with the job.
    ///
    /// # Parameters
    ///
    /// * `spider_job_id` - The Spider job of the job to resume.
    ///
    /// # Errors
    ///
    /// Returns an error if the job can't be driven to a terminal status, e.g., because a Spider
    /// request fails or its row can't be updated.
    fn recover(self, spider_job_id: SpiderJobId) -> impl Future<Output = Result<(), Error>> + Send;
}

/// Options for a job running in Spider.
pub struct SpiderOption {
    pub poll_interval: Duration,
    pub query_task_max_retry: u32,
}

/// Options for selecting the archives that a job reads.
pub struct ArchiveSelectionOptions {
    pub archive_retention_period_millisecs: Option<NonZeroU64>,
    pub max_datasets_per_query: Option<NonZeroUsize>,
}

/// Resources shared by the job handles created by the coordinator.
pub struct JobHandleContext {
    pub db_pool: MySqlPool,
    pub db_config: Database,
    pub archive_selection_options: ArchiveSelectionOptions,
    pub spider_option: SpiderOption,
}
