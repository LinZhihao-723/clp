//! Errors of query parsing and normalization.

use clp_s_resources::ErrorKind;

/// Errors raised while parsing or normalizing a query.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("syntax error at byte {position}: {message}")]
    Syntax { position: usize, message: Box<str> },

    #[error("invalid escape sequence at byte {position}")]
    InvalidEscape { position: usize },

    #[error("invalid column `{text}`")]
    InvalidColumn { text: Box<str> },

    #[error("invalid timestamp literal `{text}`")]
    InvalidTimestamp { text: Box<str> },

    #[error("a nested query cannot declare a namespace")]
    NamespaceInNestedQuery,

    #[error("query has {actual} {what}, more than the limit of {limit}")]
    LimitExceeded {
        what: &'static str,
        actual: u64,
        limit: u64,
    },
}

impl QueryError {
    /// # Returns
    ///
    /// The category this error maps onto.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        ErrorKind::InvalidQuery
    }
}
