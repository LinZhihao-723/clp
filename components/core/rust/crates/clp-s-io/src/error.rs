//! Errors raised while reading archives through byte sources.

use std::path::PathBuf;

use clp_s_format::ByteRange;
use clp_s_format::FormatError;
use clp_s_resources::ErrorKind;
use clp_s_resources::ResourceError;

/// Errors from reading archives through byte sources.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error(transparent)]
    Format(#[from] FormatError),

    #[error("i/o error reading bytes {}..{}", range.begin, range.end)]
    Io {
        range: ByteRange,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot open `{}`", path.display())]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("`{}` is a multi-file archive, which this reader does not support", path.display())]
    MultiFileArchive { path: PathBuf },

    #[error("storage read cancelled")]
    Cancelled,

    #[error(transparent)]
    Resource(#[from] ResourceError),
}

impl StorageError {
    /// # Returns
    ///
    /// The category this error maps onto.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Format(error) => error.kind(),
            Self::Io { source, .. } | Self::Open { source, .. } => {
                if std::io::ErrorKind::Interrupted == source.kind() {
                    ErrorKind::Cancelled
                } else {
                    ErrorKind::Io
                }
            }
            Self::MultiFileArchive { .. } => ErrorKind::UnsupportedArchive,
            Self::Cancelled => ErrorKind::Cancelled,
            Self::Resource(error) => error.kind(),
        }
    }
}
