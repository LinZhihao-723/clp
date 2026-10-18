//! Errors of the S3 source.

use clp_s_resources::ErrorKind;

/// Errors raised while creating or reading an S3 source.
///
/// Read failures reach [`clp_s_io::ByteSource`] callers wrapped in a [`std::io::Error`].
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum ObjectStoreError {
    #[error("the S3 source requires a multi-thread tokio runtime")]
    CurrentThreadRuntime,

    #[error("invalid S3 URL `{url}`: {reason}")]
    InvalidUrl { url: String, reason: &'static str },

    #[error("cannot stat s3://{bucket}/{key}")]
    Head {
        bucket: String,
        key: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("s3://{bucket}/{key} reports no valid content length")]
    MissingContentLength { bucket: String, key: String },

    #[error("cannot read bytes {begin}-{end} of s3://{bucket}/{key}")]
    Get {
        bucket: String,
        key: String,
        begin: u64,
        end: u64,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("unexpected response for bytes {begin}-{end} of s3://{bucket}/{key}: {reason}")]
    UnexpectedResponse {
        bucket: String,
        key: String,
        begin: u64,
        end: u64,
        reason: String,
    },

    #[error("the S3 request did not complete: its runtime shut down or the request task panicked")]
    RequestAborted,
}

impl ObjectStoreError {
    /// # Returns
    ///
    /// The category this error maps onto.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        match self {
            Self::CurrentThreadRuntime | Self::InvalidUrl { .. } => ErrorKind::InvalidArgument,
            Self::Head { .. }
            | Self::MissingContentLength { .. }
            | Self::Get { .. }
            | Self::UnexpectedResponse { .. }
            | Self::RequestAborted => ErrorKind::Io,
        }
    }
}
