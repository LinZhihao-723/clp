//! The crate-level error type for the compression coordinator.

/// Errors returned by the compression coordinator.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Failed to build or serialize the compression task graph.
    #[error("failed to build the compression task graph: {0}")]
    TaskGraph(#[from] spider_core::task::Error),

    /// Failed to msgpack-serialize a task input.
    #[error("failed to serialize a task input: {0}")]
    TaskInputSerialization(#[from] rmp_serde::encode::Error),

    /// A request to the Spider cluster failed (submission, start, or status polling).
    /// TODO: We might need more concrete error types.
    #[error("spider cluster request failed: {0}")]
    Cluster(String),
}
