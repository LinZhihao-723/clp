//! Errors of timestamp patterns.

/// Errors raised while parsing a pattern or formatting a timestamp with it.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum TimestampError {
    #[error("unsupported timestamp pattern specifier at byte {position} of `{pattern}`")]
    UnsupportedSpecifier { pattern: Box<str>, position: usize },

    #[error("invalid timestamp pattern `{pattern}`: {reason}")]
    InvalidPattern {
        pattern: Box<str>,
        reason: &'static str,
    },

    #[error("timestamp {epoch_ns} ns cannot be formatted with pattern `{pattern}`")]
    Unformattable { pattern: Box<str>, epoch_ns: i64 },
}
