//! Errors of planning and evaluation.

use clp_s_format::DictionaryKind;
use clp_s_format::FormatError;
use clp_s_resources::ErrorKind;
use clp_s_table::LayoutError;
use clp_s_table::TableError;

/// Errors raised while planning a query against one archive.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error(transparent)]
    Format(#[from] FormatError),

    #[error(transparent)]
    Layout(#[from] LayoutError),

    #[error("projection column `{column}` is invalid: {reason}")]
    InvalidProjection {
        column: Box<str>,
        reason: &'static str,
    },

    #[error("the plan needs {actual_size} bytes, more than the limit of {limit_size}")]
    LimitExceeded { actual_size: u64, limit_size: u64 },

    #[error("the {0:?} dictionary is required but not loaded")]
    DictionaryNotLoaded(DictionaryKind),

    #[error("the planner reached an inconsistent state: {reason}")]
    Internal { reason: &'static str },
}

impl PlanError {
    /// # Returns
    ///
    /// The category this error maps onto.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        match self {
            Self::Format(error) => error.kind(),
            Self::Layout(error) => error.kind(),
            Self::InvalidProjection { .. } => ErrorKind::InvalidArgument,
            Self::LimitExceeded { .. } => ErrorKind::ResourceExhausted,
            Self::DictionaryNotLoaded(_) | Self::Internal { .. } => ErrorKind::Internal,
        }
    }
}

/// Errors raised while compiling or evaluating a table filter.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error(transparent)]
    Table(#[from] TableError),

    #[error(transparent)]
    Layout(#[from] LayoutError),

    #[error("the plan does not match the table layout: {reason}")]
    PlanMismatch { reason: &'static str },

    #[error("variable-dictionary entry {id} of a dictionary-float column is not a float")]
    InvalidDictionaryFloat { id: u64 },
}

impl EvalError {
    /// # Returns
    ///
    /// The category this error maps onto.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        match self {
            Self::Table(error) => error.kind(),
            Self::Layout(error) => error.kind(),
            Self::PlanMismatch { .. } => ErrorKind::Internal,
            Self::InvalidDictionaryFloat { .. } => ErrorKind::CorruptArchive,
        }
    }
}
