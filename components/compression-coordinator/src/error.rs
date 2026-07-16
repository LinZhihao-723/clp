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
}
