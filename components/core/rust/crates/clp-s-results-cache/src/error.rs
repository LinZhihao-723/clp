//! Errors of the results-cache sink.

/// Errors raised while connecting to the results cache.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum ResultsCacheError {
    #[error("the results-cache sink requires a multi-thread tokio runtime")]
    CurrentThreadRuntime,

    #[error("the results-cache batch size must be positive")]
    InvalidBatchSize,

    #[error("invalid results-cache URI")]
    InvalidUri(#[source] mongodb::error::Error),

    #[error("the results-cache URI names no database")]
    MissingDatabase,
}
