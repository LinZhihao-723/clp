//! Errors of wire encoding.

/// Errors raised while encoding wire values.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("msgpack encoding failed: {0}")]
    Msgpack(Box<str>),
}
