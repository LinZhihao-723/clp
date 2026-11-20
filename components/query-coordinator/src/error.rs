//! The crate-level error type for the query coordinator.

use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::job_config::QueryJobStatus;
use spider_core::types::resource_group::ExternalResourceGroupCredentialsError;

/// Errors returned by the query coordinator.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid coordinator configuration: {0}")]
    InvalidConfiguration(String),

    #[error("invalid Spider endpoint: {0}")]
    InvalidEndpoint(String),

    #[error("invalid job configuration: {0}")]
    InvalidJobConfig(String),

    #[error("invalid job status transition from {from:?} to {to:?}")]
    InvalidJobStatusTransition {
        from: QueryJobStatus,
        to: QueryJobStatus,
    },

    #[error("the job has already been cancelled")]
    JobCancelled,

    #[error("metadata corrupted for job {0}: its row no longer exists")]
    JobMetadataCorrupted(QueryJobId),

    #[error("failed to load the resource group credentials: {0}")]
    ResourceGroupCredentials(#[from] ExternalResourceGroupCredentialsError),

    #[error("semaphore error: {0}")]
    Semaphore(String),

    #[error("spider request failure: {0}")]
    SpiderClient(#[from] spider_client::error::ClientError),

    #[error("sqlx error: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("failed to build the task graph: {0}")]
    TaskGraph(#[from] spider_core::task::Error),

    #[error("failed to serialize a task input: {0}")]
    TaskInputSerialization(#[from] spider_core::types::io::TaskGraphInputError),

    #[error("number of query tasks {0} exceeds `i32::MAX`")]
    TooManyQueryTasks(usize),
}
