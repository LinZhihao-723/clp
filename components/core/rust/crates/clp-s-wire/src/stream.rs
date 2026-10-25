//! The protocol-v2 streaming search output (`OutputHandlerImpl.cpp`, `NetworkOutputHandler`): a
//! handshake followed by one frame per result and a trailer counting them, read by the
//! search-result listener, which answers with an acknowledgement.

use crate::WireError;
use crate::encode::msgpack_error;
use crate::encode::write_message;
use crate::encode::write_str;

/// The protocol version a [`StreamHandshake`] announces.
pub const STREAM_PROTOCOL_VERSION: u8 = 2;

/// The handshake that opens a stream: the msgpack array
/// `[version, session_token, task_index, archive_id]`, byte-compatible with C++.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamHandshake<'handshake> {
    pub session_token: &'handshake str,
    pub task_index: u64,
    pub archive_id: &'handshake str,
}

impl StreamHandshake<'_> {
    /// Appends the msgpack encoding of the handshake to `output`. On failure, `output` is left as
    /// it was.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`WireError::Msgpack`] if a string is longer than msgpack's `u32` length limit.
    pub fn encode(&self, output: &mut Vec<u8>) -> Result<(), WireError> {
        let begin = output.len();
        let result = self.encode_fields(output);
        if result.is_err() {
            output.truncate(begin);
        }
        result
    }

    /// Appends the four fields of the handshake to `output`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`WireError::Msgpack`] if a string is longer than msgpack's `u32` length limit.
    fn encode_fields(&self, output: &mut Vec<u8>) -> Result<(), WireError> {
        const NUM_FIELDS: u32 = 4;

        rmp::encode::write_array_len(output, NUM_FIELDS).map_err(msgpack_error)?;
        rmp::encode::write_uint(output, u64::from(STREAM_PROTOCOL_VERSION))
            .map_err(msgpack_error)?;
        write_str(output, "session_token", self.session_token)?;
        rmp::encode::write_uint(output, self.task_index).map_err(msgpack_error)?;
        write_str(output, "archive_id", self.archive_id)?;
        Ok(())
    }
}

/// One result frame of a stream: the msgpack array `[result_index, timestamp, message]`,
/// byte-compatible with C++.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamResult<'result> {
    /// The position of the result in its stream, counted from 0.
    pub result_index: u64,

    /// Epoch milliseconds ([`crate::epoch_ns_to_wire_ms`]); 0 for a record without timestamp.
    pub timestamp_ms: i64,

    /// The record's JSON text without a trailing newline. It is encoded like the C++ message, as
    /// the text followed by `\n`; invalid UTF-8 is replaced with U+FFFD.
    pub message: &'result [u8],
}

impl StreamResult<'_> {
    /// Appends the msgpack encoding of the frame to `output`. On failure, `output` is left as it
    /// was.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`WireError::Msgpack`] if the message is longer than msgpack's `u32` length limit.
    pub fn encode(&self, output: &mut Vec<u8>) -> Result<(), WireError> {
        let begin = output.len();
        let result = self.encode_fields(output);
        if result.is_err() {
            output.truncate(begin);
        }
        result
    }

    /// Appends the three fields of the frame to `output`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`WireError::Msgpack`] if the message is longer than msgpack's `u32` length limit.
    fn encode_fields(&self, output: &mut Vec<u8>) -> Result<(), WireError> {
        const NUM_FIELDS: u32 = 3;

        rmp::encode::write_array_len(output, NUM_FIELDS).map_err(msgpack_error)?;
        rmp::encode::write_uint(output, self.result_index).map_err(msgpack_error)?;
        rmp::encode::write_sint(output, self.timestamp_ms).map_err(msgpack_error)?;
        write_message(output, "message", self.message)?;
        Ok(())
    }
}

/// The last frame of a stream: the msgpack array `[num_results]`, with the count in its smallest
/// unsigned encoding, byte-compatible with C++.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamTrailer {
    /// The number of result frames the stream sent.
    pub num_results: u64,
}

impl StreamTrailer {
    /// Appends the msgpack encoding of the trailer to `output`. On failure, `output` is left as it
    /// was.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::encode_fields`]'s return values on failure.
    pub fn encode(self, output: &mut Vec<u8>) -> Result<(), WireError> {
        let begin = output.len();
        let result = self.encode_fields(output);
        if result.is_err() {
            output.truncate(begin);
        }
        result
    }

    /// Appends the single field of the trailer to `output`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`WireError::Msgpack`] if writing to `output` fails.
    fn encode_fields(self, output: &mut Vec<u8>) -> Result<(), WireError> {
        const NUM_FIELDS: u32 = 1;

        rmp::encode::write_array_len(output, NUM_FIELDS).map_err(msgpack_error)?;
        rmp::encode::write_uint(output, self.num_results).map_err(msgpack_error)?;
        Ok(())
    }
}

/// The listener's acknowledgement of a stream: the msgpack array `[num_results]`, with the count
/// always encoded as a `uint 64`, so that it is exactly [`Self::ENCODED_LEN`] bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamAck {
    /// The number of results the acknowledged stream's trailer counted.
    pub num_results: u64,
}

impl StreamAck {
    /// The size of every encoded acknowledgement.
    pub const ENCODED_LEN: usize = 10;
    /// The bytes every encoded acknowledgement starts with: a one-element array marker and a
    /// `uint 64` marker.
    const PREFIX: [u8; 2] = [0x91, 0xcf];

    /// # Returns
    ///
    /// The encoded acknowledgement.
    #[must_use]
    pub fn encode(self) -> [u8; Self::ENCODED_LEN] {
        let mut encoded = [0; Self::ENCODED_LEN];
        encoded[..Self::PREFIX.len()].copy_from_slice(&Self::PREFIX);
        encoded[Self::PREFIX.len()..].copy_from_slice(&self.num_results.to_be_bytes());
        encoded
    }

    /// Decodes an acknowledgement.
    ///
    /// # Returns
    ///
    /// The decoded acknowledgement on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`WireError::MalformedAck`] if `encoded` doesn't start with the acknowledgement's prefix.
    pub fn decode(encoded: &[u8; Self::ENCODED_LEN]) -> Result<Self, WireError> {
        let [array_marker, uint64_marker, count @ ..] = *encoded;
        if Self::PREFIX != [array_marker, uint64_marker] {
            return Err(WireError::MalformedAck(*encoded));
        }
        Ok(Self {
            num_results: u64::from_be_bytes(count),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::StreamAck;
    use super::StreamHandshake;
    use super::StreamResult;
    use super::StreamTrailer;
    use crate::WireError;

    const BASE: StreamResult<'static> = StreamResult {
        result_index: 0,
        timestamp_ms: 0,
        message: b"{}",
    };

    /// # Returns
    ///
    /// The encoding of `result`.
    fn encode(result: &StreamResult<'_>) -> Vec<u8> {
        let mut output = Vec::new();
        result.encode(&mut output).expect("the result encodes");
        output
    }

    #[test]
    fn handshakes_are_the_cpp_bytes() -> anyhow::Result<()> {
        const SESSION_TOKEN: &str = "6f1c1c64-1e0a-4be8-a7a6-6a0e4d3c2b1a";
        const ARCHIVE_ID: &str = "018e90e5-8b2a-4a61-a2fc-cac799936caf";

        let mut output = vec![0xc0];
        StreamHandshake {
            session_token: SESSION_TOKEN,
            task_index: 300,
            archive_id: ARCHIVE_ID,
        }
        .encode(&mut output)?;
        let mut expected = vec![0xc0, 0x94, 0x02, 0xd9, 0x24];
        expected.extend_from_slice(SESSION_TOKEN.as_bytes());
        expected.extend_from_slice(&[0xcd, 0x01, 0x2c, 0xd9, 0x24]);
        expected.extend_from_slice(ARCHIVE_ID.as_bytes());
        assert_eq!(output, expected);

        let decoded: (u8, String, u64, String) = rmp_serde::from_slice(&output[1..])?;
        assert_eq!(
            decoded,
            (2, SESSION_TOKEN.to_owned(), 300, ARCHIVE_ID.to_owned())
        );
        Ok(())
    }

    #[test]
    fn result_indices_use_the_smallest_unsigned_representation() {
        for (result_index, expected_prefix) in [
            (0_u64, vec![0x00]),
            (127, vec![0x7f]),
            (128, vec![0xcc, 0x80]),
            (65_535, vec![0xcd, 0xff, 0xff]),
            (65_536, vec![0xce, 0x00, 0x01, 0x00, 0x00]),
            (
                1 << 32,
                vec![0xcf, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00],
            ),
        ] {
            let encoded = encode(&StreamResult {
                result_index,
                ..BASE
            });
            assert_eq!(encoded[0], 0x93, "{result_index}");
            assert_eq!(
                &encoded[1..=expected_prefix.len()],
                &expected_prefix[..],
                "{result_index}"
            );
            assert_eq!(
                encoded[expected_prefix.len() + 1],
                0x00,
                "{result_index}: the timestamp follows"
            );
        }
    }

    #[test]
    fn timestamps_use_the_smallest_signed_representation() {
        for (timestamp_ms, expected_timestamp) in [
            (0_i64, vec![0x00]),
            (
                1_700_000_010_000,
                vec![0xcf, 0x00, 0x00, 0x01, 0x8b, 0xcf, 0xe5, 0x8f, 0x10],
            ),
            (-1, vec![0xff]),
            (-33, vec![0xd0, 0xdf]),
            (-129, vec![0xd1, 0xff, 0x7f]),
            (-32_769, vec![0xd2, 0xff, 0xff, 0x7f, 0xff]),
            (
                -2_147_483_649,
                vec![0xd3, 0xff, 0xff, 0xff, 0xff, 0x7f, 0xff, 0xff, 0xff],
            ),
        ] {
            let encoded = encode(&StreamResult {
                result_index: 5,
                timestamp_ms,
                ..BASE
            });
            assert_eq!(&encoded[..2], &[0x93, 0x05], "{timestamp_ms}");
            assert_eq!(
                &encoded[2..2 + expected_timestamp.len()],
                &expected_timestamp[..],
                "{timestamp_ms}"
            );
        }
    }

    #[test]
    fn messages_are_newline_terminated_strings() -> anyhow::Result<()> {
        assert_eq!(encode(&BASE), [0x93, 0x00, 0x00, 0xa3, b'{', b'}', b'\n']);
        for (message_size, expected_header) in [
            (30, vec![0xbf]),
            (31, vec![0xd9, 0x20]),
            (254, vec![0xd9, 0xff]),
            (255, vec![0xda, 0x01, 0x00]),
        ] {
            let message = vec![b'x'; message_size];
            let encoded = encode(&StreamResult {
                message: &message,
                ..BASE
            });
            assert_eq!(
                &encoded[3..3 + expected_header.len()],
                &expected_header[..],
                "{message_size}"
            );
            let decoded: (u64, i64, String) = rmp_serde::from_slice(&encoded)?;
            assert_eq!(decoded.2.len(), message_size + 1);
            assert!(decoded.2.ends_with('\n'));
        }
        Ok(())
    }

    #[test]
    fn invalid_utf8_is_replaced_with_the_replacement_character() -> anyhow::Result<()> {
        let encoded = encode(&StreamResult {
            message: b"{\"a\":\"\xff\xfe\"}",
            ..BASE
        });
        let decoded: (u64, i64, String) = rmp_serde::from_slice(&encoded)?;
        assert_eq!(decoded.2, "{\"a\":\"\u{fffd}\u{fffd}\"}\n");
        Ok(())
    }

    #[test]
    fn frames_round_trip_and_append() -> anyhow::Result<()> {
        let mut output = vec![0xc0];
        StreamResult {
            result_index: 2990,
            timestamp_ms: -1_000,
            message: "{\"msg\":\"café\"}".as_bytes(),
        }
        .encode(&mut output)?;
        assert_eq!(output[0], 0xc0);
        let decoded: (u64, i64, String) = rmp_serde::from_slice(&output[1..])?;
        assert_eq!(decoded, (2990, -1_000, "{\"msg\":\"café\"}\n".to_owned()));
        Ok(())
    }

    #[test]
    fn trailers_use_the_smallest_unsigned_representation() -> anyhow::Result<()> {
        for (num_results, expected) in [
            (0_u64, vec![0x91, 0x00]),
            (127, vec![0x91, 0x7f]),
            (128, vec![0x91, 0xcc, 0x80]),
            (65_535, vec![0x91, 0xcd, 0xff, 0xff]),
            (65_536, vec![0x91, 0xce, 0x00, 0x01, 0x00, 0x00]),
            (
                1 << 32,
                vec![0x91, 0xcf, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00],
            ),
        ] {
            let mut output = vec![0xc0];
            StreamTrailer { num_results }.encode(&mut output)?;
            assert_eq!(output[0], 0xc0, "{num_results}");
            assert_eq!(&output[1..], &expected[..], "{num_results}");
            let decoded: (u64,) = rmp_serde::from_slice(&output[1..])?;
            assert_eq!(decoded, (num_results,));
        }
        Ok(())
    }

    #[test]
    fn acks_are_ten_bytes_with_a_uint64_count() -> anyhow::Result<()> {
        for (num_results, expected) in [
            (0_u64, [0x91, 0xcf, 0, 0, 0, 0, 0, 0, 0, 0]),
            (7, [0x91, 0xcf, 0, 0, 0, 0, 0, 0, 0, 7]),
            (
                0x0102_0304_0506_0708,
                [0x91, 0xcf, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08],
            ),
            (
                u64::MAX,
                [0x91, 0xcf, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            ),
        ] {
            let encoded = StreamAck { num_results }.encode();
            assert_eq!(encoded, expected, "{num_results}");
            assert_eq!(StreamAck::decode(&encoded)?, StreamAck { num_results });
            let decoded: (u64,) = rmp_serde::from_slice(&encoded)?;
            assert_eq!(decoded, (num_results,));
        }
        Ok(())
    }

    #[test]
    fn acks_with_another_prefix_are_rejected() {
        for encoded in [
            [0x91, 0xce, 0, 0, 0, 0, 0, 0, 0, 7],
            [0x92, 0xcf, 0, 0, 0, 0, 0, 0, 0, 7],
            [0x07, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        ] {
            let error = StreamAck::decode(&encoded).expect_err("the prefix is wrong");
            assert!(
                matches!(error, WireError::MalformedAck(bytes) if bytes == encoded),
                "{error:?}"
            );
        }
    }
}
