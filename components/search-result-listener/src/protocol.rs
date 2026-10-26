//! The wire protocols that search tasks use to stream their results.
//!
//! A connection starts with a msgpack handshake that names the protocol version. Under protocol
//! v2, msgpack arrays without length prefixes follow: one frame per result and a trailer that
//! counts them. Under protocol v3, an Arrow IPC stream of the results follows, then the same
//! trailer. The task then shuts down its side of the connection and waits for the listener's
//! acknowledgement before it reports success.

use bytes::Buf;
use bytes::BytesMut;
use clp_rust_utils::job_config::SessionToken;
use clp_rust_utils::task_io::query::QueryTaskIndex;
use clp_rust_utils::types::ArchiveId;
use rmp::decode::Bytes;
use rmp::decode::NumValueReadError;
use rmp::decode::ValueReadError;
use rmp::decode::bytes::BytesReadError;

use crate::error::ProtocolError;

/// A protocol version the listener accepts, with its value in the handshake as its discriminant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u64)]
pub enum ProtocolVersion {
    /// Connections send one msgpack frame per result.
    V2 = 2,

    /// Connections send their results as an Arrow IPC stream between the handshake and the
    /// trailer.
    V3 = 3,
}

impl TryFrom<u64> for ProtocolVersion {
    type Error = ProtocolError;

    fn try_from(version: u64) -> Result<Self, ProtocolError> {
        match version {
            2 => Ok(Self::V2),
            3 => Ok(Self::V3),
            _ => Err(ProtocolError::UnsupportedVersion(version)),
        }
    }
}

/// The frame a search task sends first on every connection.
#[derive(Debug, Eq, PartialEq)]
pub struct Handshake {
    pub version: ProtocolVersion,
    pub session_token: SessionToken,

    /// The position of the archive among the archives that its query job searches.
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
    /// * [`ProtocolError::UnsupportedVersion`] if the handshake's version isn't a
    ///   [`ProtocolVersion`].
    /// * [`ProtocolError::MalformedFrame`] if the bytes don't form a handshake.
    /// * [`ProtocolError::InvalidSessionToken`] if the session token isn't a valid UUID.
    /// * [`ProtocolError::InvalidArchiveId`] if the archive ID isn't a valid UUID.
    pub fn decode(buffer: &mut BytesMut) -> Result<Option<Self>, ProtocolError> {
        decode_frame(buffer, |reader| {
            let num_fields = rmp::decode::read_array_len(reader)?;
            let version = ProtocolVersion::try_from(rmp::decode::read_int::<u64, _>(reader)?)?;
            expect_num_fields(num_fields, NUM_HANDSHAKE_FIELDS)?;
            let session_token =
                SessionToken::parse_str(read_str(reader, MAX_HANDSHAKE_STRING_LEN)?)
                    .map_err(ProtocolError::from)?;
            let task_index = rmp::decode::read_int(reader)?;
            let archive_id = read_str(reader, MAX_HANDSHAKE_STRING_LEN)?
                .parse::<ArchiveId>()
                .map_err(ProtocolError::from)?;
            Ok(Self {
                version,
                session_token,
                task_index,
                archive_id,
            })
        })
    }
}

/// A frame that follows the handshake.
#[derive(Debug, Eq, PartialEq)]
pub enum Frame {
    /// One result of the task.
    Result(ResultFrame),

    /// The last frame, sent once the task has sent every result.
    Trailer(Trailer),
}

impl Frame {
    /// Decodes a frame from the front of `buffer`, consuming its bytes. The number of fields of
    /// the frame's array tells a result from a trailer.
    ///
    /// # Returns
    ///
    /// On success:
    ///
    /// * The decoded frame.
    /// * `None` if `buffer` doesn't hold a complete frame yet, leaving `buffer` untouched.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ProtocolError::MalformedFrame`] if the bytes don't form a v2 result frame or trailer.
    pub fn decode(buffer: &mut BytesMut) -> Result<Option<Self>, ProtocolError> {
        decode_frame(buffer, |reader| {
            match rmp::decode::read_array_len(reader)? {
                NUM_RESULT_FIELDS => {
                    let result_index = rmp::decode::read_int(reader)?;
                    let timestamp = rmp::decode::read_int(reader)?;
                    let message = read_str(reader, usize::MAX)?.to_owned();
                    Ok(Self::Result(ResultFrame {
                        result_index,
                        timestamp,
                        message,
                    }))
                }
                NUM_TRAILER_FIELDS => Ok(Self::Trailer(Trailer {
                    num_results: rmp::decode::read_int(reader)?,
                })),
                num_fields => Err(ProtocolError::MalformedFrame(format!(
                    "expected {NUM_RESULT_FIELDS} fields for a result or {NUM_TRAILER_FIELDS} for \
                     a trailer, got {num_fields}"
                ))
                .into()),
            }
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

/// The frame a search task sends after its last result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Trailer {
    /// The number of results the task sent on the connection.
    pub num_results: u64,
}

/// The acknowledgement the listener sends once it has taken responsibility for every result of a
/// connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ack {
    /// The number of results the connection's trailer counted.
    pub num_results: u64,
}

impl Ack {
    /// The size of every encoded acknowledgement.
    pub const ENCODED_LEN: usize = 10;

    /// # Returns
    ///
    /// The msgpack array `[num_results]`, with the count always encoded as a `uint 64`.
    #[must_use]
    pub fn encode(self) -> [u8; Self::ENCODED_LEN] {
        let mut encoded = [0; Self::ENCODED_LEN];
        encoded[0] = ACK_ARRAY_MARKER;
        encoded[1] = ACK_UINT64_MARKER;
        encoded[2..].copy_from_slice(&self.num_results.to_be_bytes());
        encoded
    }
}

const NUM_HANDSHAKE_FIELDS: u32 = 4;
const NUM_RESULT_FIELDS: u32 = 3;
const NUM_TRAILER_FIELDS: u32 = 1;

/// The msgpack marker of an array of one element.
const ACK_ARRAY_MARKER: u8 = 0x91;

/// The msgpack marker of a `uint 64`.
const ACK_UINT64_MARKER: u8 = 0xcf;

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
/// # Type Parameters
///
/// * `FrameType` - The type of the frame that `decode` produces.
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
fn decode_frame<FrameType>(
    buffer: &mut BytesMut,
    decode: impl FnOnce(&mut Bytes<'_>) -> Result<FrameType, DecodeError>,
) -> Result<Option<FrameType>, ProtocolError> {
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
    use clp_rust_utils::job_config::SessionToken;
    use clp_rust_utils::types::ArchiveId;

    use super::Ack;
    use super::Frame;
    use super::Handshake;
    use super::ProtocolVersion;
    use super::ResultFrame;
    use super::Trailer;
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

    /// # Returns
    ///
    /// The bytes of a trailer counting `num_results`, encoded the way `clp-s` does.
    fn encode_trailer(num_results: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        rmp::encode::write_array_len(&mut bytes, 1).expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_uint(&mut bytes, num_results)
            .expect("writing to a `Vec` shouldn't fail");
        bytes
    }

    /// # Returns
    ///
    /// The frames decoded from `bytes`, which must hold only complete frames.
    ///
    /// # Panics
    ///
    /// Panics if `bytes` doesn't hold only complete, valid frames.
    fn decode_frames(bytes: &[u8]) -> Vec<Frame> {
        let mut buffer = BytesMut::from(bytes);
        let mut frames = Vec::new();
        while let Some(frame) = Frame::decode(&mut buffer).expect("frames should be valid") {
            frames.push(frame);
        }
        assert_eq!(&buffer[..], &[] as &[u8]);
        frames
    }

    #[test]
    fn handshake_decodes_only_once_complete() {
        let encoded = encode_handshake(ProtocolVersion::V2 as u64);
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
                version: ProtocolVersion::V2,
                session_token: SessionToken::parse_str(SESSION_TOKEN).expect("valid UUID"),
                task_index: 7,
                archive_id: ARCHIVE_ID.parse::<ArchiveId>().expect("valid archive UUID"),
            })
        );
        assert_eq!(&buffer[..], &[0x93]);
    }

    #[test]
    fn handshake_of_the_arrow_protocol_carries_its_version() {
        let mut buffer = BytesMut::from(&encode_handshake(ProtocolVersion::V3 as u64)[..]);
        let handshake = Handshake::decode(&mut buffer)
            .expect("the handshake should be valid")
            .expect("the handshake should be complete");
        assert_eq!(handshake.version, ProtocolVersion::V3);
        assert_eq!(handshake.task_index, 7);
    }

    #[test]
    fn handshake_of_another_version_is_rejected_before_its_other_fields() {
        for version in [1, 4] {
            let mut buffer = BytesMut::from(&encode_handshake(version)[..2]);
            let error = Handshake::decode(&mut buffer).expect_err("the version should be rejected");
            assert!(
                matches!(error, ProtocolError::UnsupportedVersion(rejected) if version == rejected),
                "unexpected error: {error:?}"
            );
        }
    }

    #[test]
    fn handshake_with_an_overlong_string_is_rejected_before_it_arrives() {
        let mut bytes = Vec::new();
        rmp::encode::write_array_len(&mut bytes, 4).expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_uint(&mut bytes, ProtocolVersion::V2 as u64)
            .expect("writing to a `Vec` shouldn't fail");
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

        let result = |result_index: u64, timestamp: i64, message: &str| {
            Frame::Result(ResultFrame {
                result_index,
                timestamp,
                message: message.to_owned(),
            })
        };
        assert_eq!(
            decode_frames(&bytes),
            [
                result(0, 0, "message\n"),
                result(300, -5, "message\n"),
                result(70_000, 1_700_000_000_000, "message\n"),
                result(3, 1_700_000_000_001, "last\n"),
            ]
        );
    }

    #[test]
    fn result_with_a_timestamp_beyond_int64_is_rejected() {
        let mut bytes = Vec::new();
        rmp::encode::write_array_len(&mut bytes, 3).expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_uint(&mut bytes, 0).expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_u64(&mut bytes, u64::MAX).expect("writing to a `Vec` shouldn't fail");
        let mut buffer = BytesMut::from(&bytes[..]);

        let error = Frame::decode(&mut buffer).expect_err("the timestamp should be rejected");
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

        let error = Frame::decode(&mut buffer).expect_err("invalid UTF-8 should be rejected");
        assert!(
            matches!(error, ProtocolError::MalformedFrame(_)),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn trailer_decodes_at_every_uint_width_only_once_complete() {
        for (num_results, encoded_len) in [
            (0_u64, 2),
            (127, 2),
            (128, 3),
            (255, 3),
            (256, 4),
            (65_535, 4),
            (65_536, 6),
            (u64::from(u32::MAX), 6),
            (u64::from(u32::MAX) + 1, 10),
            (u64::MAX, 10),
        ] {
            let encoded = encode_trailer(num_results);
            assert_eq!(encoded.len(), encoded_len, "{num_results}");
            let mut buffer = BytesMut::new();
            for &byte in &encoded[..encoded.len() - 1] {
                buffer.extend_from_slice(&[byte]);
                assert_eq!(
                    Frame::decode(&mut buffer).expect("a trailer prefix should be valid"),
                    None,
                    "{num_results}"
                );
            }
            assert_eq!(buffer.len(), encoded.len() - 1, "{num_results}");

            buffer.extend_from_slice(&encoded[encoded.len() - 1..]);
            buffer.extend_from_slice(&[0x93]);
            assert_eq!(
                Frame::decode(&mut buffer).expect("the trailer should be valid"),
                Some(Frame::Trailer(Trailer { num_results })),
                "{num_results}"
            );
            assert_eq!(&buffer[..], &[0x93], "{num_results}");
        }
    }

    #[test]
    fn trailer_follows_the_results_it_counts() {
        let mut bytes = Vec::new();
        rmp::encode::write_array_len(&mut bytes, 3).expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_uint(&mut bytes, 0).expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_sint(&mut bytes, 1).expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_str(&mut bytes, "only\n").expect("writing to a `Vec` shouldn't fail");
        bytes.extend(encode_trailer(1));

        assert_eq!(
            decode_frames(&bytes),
            [
                Frame::Result(ResultFrame {
                    result_index: 0,
                    timestamp: 1,
                    message: "only\n".to_owned(),
                }),
                Frame::Trailer(Trailer { num_results: 1 }),
            ]
        );
    }

    #[test]
    fn malformed_frames_are_rejected() {
        let mut malformed_frames = Vec::new();
        for num_fields in [0, 2, 4] {
            let mut bytes = Vec::new();
            rmp::encode::write_array_len(&mut bytes, num_fields)
                .expect("writing to a `Vec` shouldn't fail");
            rmp::encode::write_uint(&mut bytes, 0).expect("writing to a `Vec` shouldn't fail");
            malformed_frames.push(bytes);
        }
        let mut negative_count = Vec::new();
        rmp::encode::write_array_len(&mut negative_count, 1)
            .expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_sint(&mut negative_count, -1)
            .expect("writing to a `Vec` shouldn't fail");
        malformed_frames.push(negative_count);
        let mut string_count = Vec::new();
        rmp::encode::write_array_len(&mut string_count, 1)
            .expect("writing to a `Vec` shouldn't fail");
        rmp::encode::write_str(&mut string_count, "1").expect("writing to a `Vec` shouldn't fail");
        malformed_frames.push(string_count);
        let mut not_an_array = Vec::new();
        rmp::encode::write_uint(&mut not_an_array, 1).expect("writing to a `Vec` shouldn't fail");
        malformed_frames.push(not_an_array);

        for bytes in malformed_frames {
            let mut buffer = BytesMut::from(&bytes[..]);
            let error = Frame::decode(&mut buffer).expect_err("the frame should be rejected");
            assert!(
                matches!(error, ProtocolError::MalformedFrame(_)),
                "{bytes:02x?}: unexpected error: {error:?}"
            );
        }
    }

    #[test]
    fn ack_is_a_one_element_array_of_a_uint64() {
        for (num_results, count_bytes) in [
            (0_u64, [0, 0, 0, 0, 0, 0, 0, 0]),
            (1, [0, 0, 0, 0, 0, 0, 0, 1]),
            (20_000, [0, 0, 0, 0, 0, 0, 0x4e, 0x20]),
            (
                0x0102_0304_0506_0708,
                [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08],
            ),
            (u64::MAX, [0xff; 8]),
        ] {
            let encoded = Ack { num_results }.encode();
            assert_eq!(encoded[..2], [0x91, 0xcf], "{num_results}");
            assert_eq!(encoded[2..], count_bytes, "{num_results}");

            let mut reader = rmp::decode::Bytes::new(&encoded);
            assert_eq!(
                rmp::decode::read_array_len(&mut reader).expect("the ack should be an array"),
                1
            );
            assert_eq!(
                rmp::decode::read_u64(&mut reader).expect("the count should be a `uint 64`"),
                num_results
            );
            assert_eq!(reader.remaining_slice(), &[] as &[u8]);
        }
    }
}
