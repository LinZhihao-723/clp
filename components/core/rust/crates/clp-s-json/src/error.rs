//! Errors of JSON rendering.

use clp_s_format::FormatError;
use clp_s_resources::ErrorKind;
use clp_s_table::ColumnIndex;
use clp_s_table::TableError;
use clp_s_timestamp::TimestampError;

/// Errors raised while compiling a template or rendering records.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum JsonError {
    #[error(transparent)]
    Table(#[from] TableError),

    #[error(transparent)]
    Format(#[from] FormatError),

    #[error(transparent)]
    Timestamp(#[from] TimestampError),

    #[error("the record buffer would exceed {limit_size} bytes")]
    BufferOverflow { limit_size: u64 },

    #[error("column {0:?} does not hold the values its template slot was compiled for")]
    UnexpectedColumn(ColumnIndex),
}

impl JsonError {
    /// # Returns
    ///
    /// The category this error maps onto.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        match self {
            Self::Table(error) => error.kind(),
            Self::Format(error) => error.kind(),
            Self::Timestamp(_) => ErrorKind::UnsupportedArchive,
            Self::BufferOverflow { .. } => ErrorKind::ResourceExhausted,
            Self::UnexpectedColumn(_) => ErrorKind::Internal,
        }
    }
}
