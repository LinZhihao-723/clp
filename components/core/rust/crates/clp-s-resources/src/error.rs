//! Resource errors and the error categories shared by every layer.

/// The category every reader error maps onto; C status codes, Python exceptions and CLI exit codes
/// are derived from it.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// The caller passed an invalid argument or configuration.
    InvalidArgument,

    /// The query text or expression is invalid.
    InvalidQuery,

    /// Reading the archive failed.
    Io,

    /// The archive bytes are corrupt.
    CorruptArchive,

    /// The archive is valid but uses a version, layout or feature this reader does not support.
    UnsupportedArchive,

    /// The caller cancelled the operation.
    Cancelled,

    /// A configured limit or the memory budget was exceeded, or an allocation failed.
    ResourceExhausted,

    /// A sink or downstream consumer failed.
    Sink,

    /// An internal invariant was violated.
    Internal,
}

/// Errors raised by the resources of this crate.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum ResourceError {
    #[error("operation cancelled")]
    Cancelled,

    #[error("memory budget of {limit_size} bytes cannot admit {requested_size} more bytes")]
    BudgetExhausted {
        requested_size: u64,
        limit_size: u64,
    },

    #[error("failed to allocate {size} bytes")]
    AllocationFailed { size: u64 },
}

impl ResourceError {
    /// # Returns
    ///
    /// The category this error maps onto.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        match self {
            Self::Cancelled => ErrorKind::Cancelled,
            Self::BudgetExhausted { .. } | Self::AllocationFailed { .. } => {
                ErrorKind::ResourceExhausted
            }
        }
    }
}
