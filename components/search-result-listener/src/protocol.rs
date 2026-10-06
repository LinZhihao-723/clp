//! Decoding of wire protocol v1, which `clp-s` uses to stream a search task's results.
//!
//! A connection carries msgpack arrays without length prefixes: one handshake, followed by one
//! frame per result. `clp-s` closes the connection once it has sent every result.

use bytes::Buf;
use bytes::BytesMut;
use clp_rust_utils::task_io::query::QueryTaskIndex;
use clp_rust_utils::types::ArchiveId;
use rmp::decode::Bytes;
use rmp::decode::NumValueReadError;
use rmp::decode::ValueReadError;
use rmp::decode::bytes::BytesReadError;

use crate::SessionToken;
use crate::error::ProtocolError;

pub const PROTOCOL_VERSION: u64 = 1;

/// The frame a search task sends first on every connection.
#[derive(Debug, Eq, PartialEq)]
pub struct Handshake {
    pub session_token: SessionToken,
    pub task_index: QueryTaskIndex,
    pub archive_id: ArchiveId,
}

impl Handshake {
    /// Decodes a handshake from the front of `buffer`, consuming its bytes.
    ///
    /// # Returns
    ///
    /// On success:
    ///
    /// * The decoded handshake.
    /// * `None` if `buffer` doesn't hold a complete handshake yet, leaving `buffer` untouched.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ProtocolError::UnsupportedVersion`] if the handshake's version isn't
    ///   [`PROTOCOL_VERSION`].
    /// * [`ProtocolError::MalformedFrame`] if the bytes don't form a v1 handshake.
    /// * [`ProtocolError::InvalidSessionToken`] if the session token isn't a valid UUID.
    /// * [`ProtocolError::InvalidArchiveId`] if the archive ID isn't a valid UUID.
    pub fn decode(buffer: &mut BytesMut) -> Result<Option<Self>, ProtocolError> {
        decode_frame(buffer, |reader| {
            let num_fields = rmp::decode::read_array_len(reader)?;
            let version: u64 = rmp::decode::read_int(reader)?;
            if PROTOCOL_VERSION != version {
                return Err(ProtocolError::UnsupportedVersion(version).into());
            }
            expect_num_fields(num_fields, NUM_HANDSHAKE_FIELDS)?;
            let session_token =
                SessionToken::parse_str(read_str(reader, MAX_HANDSHAKE_STRING_LEN)?)
                    .map_err(ProtocolError::from)?;
            let task_index = rmp::decode::read_int(reader)?;
            let archive_id = read_str(reader, MAX_HANDSHAKE_STRING_LEN)?
                .parse::<ArchiveId>()
                .map_err(ProtocolError::from)?;
            Ok(Self {
                session_token,
                task_index,
                archive_id,
            })
        })
    }
}

/// The frame a search task sends for each result.
#[derive(Debug, Eq, PartialEq)]
pub struct ResultFrame {
    pub result_index: u64,
    pub timestamp: i64,
    pub message: String,
}

impl ResultFrame {
    /// Decodes a result frame from the front of `buffer`, consuming its bytes.
    ///
    /// # Returns
    ///
    /// On success:
    ///
    /// * The decoded result frame.
    /// * `None` if `buffer` doesn't hold a complete result frame yet, leaving `buffer` untouched.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ProtocolError::MalformedFrame`] if the bytes don't form a v1 result frame.
    pub fn decode(buffer: &mut BytesMut) -> Result<Option<Self>, ProtocolError> {
        decode_frame(buffer, |reader| {
            expect_num_fields(rmp::decode::read_array_len(reader)?, NUM_RESULT_FIELDS)?;
            let result_index = rmp::decode::read_int(reader)?;
            let timestamp = rmp::decode::read_int(reader)?;
            let message = read_str(reader, usize::MAX)?.to_owned();
            Ok(Self {
                result_index,
                timestamp,
                message,
            })
        })
    }
}

const NUM_HANDSHAKE_FIELDS: u32 = 4;
const NUM_RESULT_FIELDS: u32 = 3;

/// The longest string a handshake may carry. Every textual UUID form fits, and the bound keeps an
/// unauthenticated peer from making the listener buffer an arbitrarily long frame.
const MAX_HANDSHAKE_STRING_LEN: usize = 64;

/// Why a frame couldn't be decoded from a buffer.
enum DecodeError {
    /// The buffer ends before the frame does.
    Incomplete,

    /// The buffer's bytes don't form a valid frame.
    Invalid(ProtocolError),
}

impl From<ProtocolError> for DecodeError {
    fn from(error: ProtocolError) -> Self {
        Self::Invalid(error)
    }
}

impl From<ValueReadError<BytesReadError>> for DecodeError {
    fn from(error: ValueReadError<BytesReadError>) -> Self {
        match error {
            ValueReadError::InvalidMarkerRead(_) | ValueReadError::InvalidDataRead(_) => {
                Self::Incomplete
            }
            ValueReadError::TypeMismatch(marker) => Self::Invalid(ProtocolError::MalformedFrame(
                format!("unexpected msgpack marker {marker:?}"),
            )),
        }
    }
}

impl From<NumValueReadError<BytesReadError>> for DecodeError {
    fn from(error: NumValueReadError<BytesReadError>) -> Self {
        match error {
            NumValueReadError::InvalidMarkerRead(_) | NumValueReadError::InvalidDataRead(_) => {
                Self::Incomplete
            }
            NumValueReadError::TypeMismatch(marker) => Self::Invalid(
                ProtocolError::MalformedFrame(format!("expected an integer, got {marker:?}")),
            ),
            NumValueReadError::OutOfRange => Self::Invalid(ProtocolError::MalformedFrame(
                "integer out of range".to_owned(),
            )),
        }
    }
}

/// Decodes one frame from the front of `buffer` with `decode`, consuming the frame's bytes only if
/// it decodes completely.
///
/// # Returns
///
/// On success:
///
/// * The decoded frame.
/// * `None` if `buffer` ends before the frame does.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards the [`ProtocolError`] of an invalid frame from `decode`.
fn decode_frame<Frame>(
    buffer: &mut BytesMut,
    decode: impl FnOnce(&mut Bytes<'_>) -> Result<Frame, DecodeError>,
) -> Result<Option<Frame>, ProtocolError> {
    let mut reader = Bytes::new(buffer);
    match decode(&mut reader) {
        Ok(frame) => {
            let frame_len = buffer.len() - reader.remaining_slice().len();
            buffer.advance(frame_len);
            Ok(Some(frame))
        }
        Err(DecodeError::Incomplete) => Ok(None),
        Err(DecodeError::Invalid(error)) => Err(error),
    }
}

/// # Errors
///
/// Returns an error if:
///
/// * [`DecodeError::Invalid`] if `num_fields` isn't `expected`.
fn expect_num_fields(num_fields: u32, expected: u32) -> Result<(), DecodeError> {
    if expected == num_fields {
        return Ok(());
    }
    Err(
        ProtocolError::MalformedFrame(format!("expected {expected} fields, got {num_fields}"))
            .into(),
    )
}

/// Reads a msgpack string of at most `max_len` bytes.
///
/// # Returns
///
/// The string, borrowed from the reader's buffer, on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`DecodeError::Incomplete`] if the reader ends before the string does.
/// * [`DecodeError::Invalid`] if:
///   * The next value isn't a string.
///   * The string is longer than `max_len` bytes.
///   * The string isn't valid UTF-8.
fn read_str<'buffer>(
    reader: &mut Bytes<'buffer>,
    max_len: usize,
) -> Result<&'buffer str, DecodeError> {
    let len = usize::try_from(rmp::decode::read_str_len(reader)?).map_err(|_| {
        ProtocolError::MalformedFrame("string length exceeds the address space".to_owned())
    })?;
    if len > max_len {
        return Err(ProtocolError::MalformedFrame(format!(
            "string of {len} bytes exceeds the limit of {max_len} bytes"
        ))
        .into());
    }
    let Some((bytes, remaining)) = reader.remaining_slice().split_at_checked(len) else {
        return Err(DecodeError::Incomplete);
    };
    *reader = Bytes::new(remaining);
    std::str::from_utf8(bytes)
        .map_err(|e| ProtocolError::MalformedFrame(format!("invalid UTF-8 string: {e}")).into())
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use clp_rust_utils::types::ArchiveId;

    use super::Handshake;
    use super::ResultFrame;
    use crate::SessionToken;
    use crate::error::ProtocolError;

    const ARCHIVE_ID: &str = "018e90e5-8b2a-4a61-a2fc-cac799936caf";
    const SESSION_TOKEN: &str = "6f1d3b52-8a4e-4c1b-9f6e-2d7a5c0b9e13";

    /// # Returns
    ///
    /// The bytes `clp-s` sends for the handshake of task 7 of archive [`ARCHIVE_ID`], with
    /// `version` as the protocol version.
    fn encode_handshake(version: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        rmp::encode::write_array_len(&mut bytes, 4).expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_uint(&mut bytes, version).expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_str(&mut bytes, SESSION_TOKEN)
            .expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_uint(&mut bytes, 7).expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_str(&mut bytes, ARCHIVE_ID).expect("writing to a `Vec` shouldn't fail");
        bytes
    }

    #[test]
    fn handshake_decodes_only_once_complete() {
        let encoded = encode_handshake(1);
        let mut buffer = BytesMut::new();
        for &byte in &encoded[..encoded.len() - 1] {
            buffer.extend_from_slice(&[byte]);
            assert_eq!(
                Handshake::decode(&mut buffer).expect("a handshake prefix should be valid"),
                None
            );
        }
        assert_eq!(buffer.len(), encoded.len() - 1);

        buffer.extend_from_slice(&encoded[encoded.len() - 1..]);
        buffer.extend_from_slice(&[0x93]);
        let handshake = Handshake::decode(&mut buffer).expect("the handshake should be valid");
        assert_eq!(
            handshake,
            Some(Handshake {
                session_token: SessionToken::parse_str(SESSION_TOKEN).expect("valid UUID"),
                task_index: 7,
                archive_id: ARCHIVE_ID.parse::<ArchiveId>().expect("valid archive UUID"),
            })
        );
        assert_eq!(&buffer[..], &[0x93]);
    }

    #[test]
    fn handshake_with_another_version_is_rejected_before_its_other_fields() {
        let mut buffer = BytesMut::from(&encode_handshake(2)[..2]);
        let error = Handshake::decode(&mut buffer).expect_err("version 2 should be rejected");
        assert!(
            matches!(error, ProtocolError::UnsupportedVersion(2)),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn handshake_with_an_overlong_string_is_rejected_before_it_arrives() {
        let mut bytes = Vec::new();
        rmp::encode::write_array_len(&mut bytes, 4).expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_uint(&mut bytes, 1).expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_str_len(&mut bytes, 1 << 20).expect("writing to a `Vec` shouldn't fail");
        let mut buffer = BytesMut::from(&bytes[..]);

        let error = Handshake::decode(&mut buffer).expect_err("a 1 MiB token should be rejected");
        assert!(
            matches!(error, ProtocolError::MalformedFrame(_)),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn result_accepts_every_integer_encoding() {
        let mut bytes = Vec::new();
        for (result_index, timestamp) in [(0_u64, 0_i64), (300, -5), (70_000, 1_700_000_000_000)] {
            rmp::encode::write_array_len(&mut bytes, 3).expect("writing to a `Vec` shouldn't fail");
            rmp::encode::write_uint(&mut bytes, result_index)
                .expect("writing to a `Vec` shouldn't fail");
            rmp::encode::write_sint(&mut bytes, timestamp)
                .expect("writing to a `Vec` shouldn't fail");
            rmp::encode::write_str(&mut bytes, "message\n")
                .expect("writing to a `Vec` shouldn't fail");
        }
        // msgpack-c packs non-negative `int64_t` values with the smallest unsigned encoding.
        rmp::encode::write_array_len(&mut bytes, 3).expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_u64(&mut bytes, 3).expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_u64(&mut bytes, 1_700_000_000_001)
            .expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_str(&mut bytes, "last\n").expect("writing to a `Vec` shouldn't fail");
        let mut buffer = BytesMut::from(&bytes[..]);

        let mut frames = Vec::new();
        while let Some(frame) = ResultFrame::decode(&mut buffer).expect("frames should be valid") {
            frames.push((frame.result_index, frame.timestamp, frame.message));
        }
        assert_eq!(
            frames,
            [
                (0, 0, "message\n".to_owned()),
                (300, -5, "message\n".to_owned()),
                (70_000, 1_700_000_000_000, "message\n".to_owned()),
                (3, 1_700_000_000_001, "last\n".to_owned()),
            ]
        );
        assert_eq!(&buffer[..], &[] as &[u8]);
    }

    #[test]
    fn result_with_a_timestamp_beyond_int64_is_rejected() {
        let mut bytes = Vec::new();
        rmp::encode::write_array_len(&mut bytes, 3).expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_uint(&mut bytes, 0).expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_u64(&mut bytes, u64::MAX).expect("writing to a `Vec` shouldn't fail");
        let mut buffer = BytesMut::from(&bytes[..]);

        let error = ResultFrame::decode(&mut buffer).expect_err("the timestamp should be rejected");
        assert!(
            matches!(error, ProtocolError::MalformedFrame(_)),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn result_with_invalid_utf8_is_rejected() {
        let mut bytes = Vec::new();
        rmp::encode::write_array_len(&mut bytes, 3).expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_uint(&mut bytes, 0).expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_sint(&mut bytes, 0).expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_str_len(&mut bytes, 1).expect("writing to a `Vec` shouldn't fail");
        bytes.push(0xff);
        let mut buffer = BytesMut::from(&bytes[..]);

        let error = ResultFrame::decode(&mut buffer).expect_err("invalid UTF-8 should be rejected");
        assert!(
            matches!(error, ProtocolError::MalformedFrame(_)),
            "unexpected error: {error:?}"
        );
    }
}
