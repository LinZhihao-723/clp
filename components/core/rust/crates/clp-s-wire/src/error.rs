//! Errors of wire encoding and decoding.

/// Errors raised while encoding or decoding wire values.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("malformed stream acknowledgement: {0:02x?}")]
    MalformedAck([u8; crate::StreamAck::ENCODED_LEN]),

    #[error("msgpack encoding failed: {0}")]
    Msgpack(Box<str>),
}
