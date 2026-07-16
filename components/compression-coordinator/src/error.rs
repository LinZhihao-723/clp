//! The crate-level error type for the compression coordinator.

/// Errors returned by the compression coordinator.
///
/// This is a starting surface: it will be extended with additional variants when the
/// [`crate::compression_job_submitter::S3CompressionJobSubmitter`] trait is implemented.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Failed to build or serialize the compression task graph.
    #[error("failed to build the compression task graph: {0}")]
    TaskGraph(#[from] spider_core::task::Error),

    /// A request to the Spider cluster failed (submission, start, or status polling).
    /// TODO: We might need more concrete error types.
    #[error("spider cluster request failed: {0}")]
    Cluster(String),

    /// The `CLP_CONFIG_PATH` environment variable (pointing at the Spider task executor config
    /// YAML) was unset or not valid Unicode.
    #[error("failed to read the `CLP_CONFIG_PATH` environment variable: {0}")]
    SpiderTaskExecutorConfigPathEnvVar(#[from] std::env::VarError),

    /// Failed to read or parse the Spider task executor config YAML file at `CLP_CONFIG_PATH`.
    #[error("failed to load the Spider task executor config: {0}")]
    SpiderTaskExecutorConfigLoad(#[from] clp_rust_utils::Error),
}
