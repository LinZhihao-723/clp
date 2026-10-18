//! The msgpack search-result tuple (`OutputHandlerImpl.cpp`, `FileOutputHandler::write`).

use crate::WireError;

/// One search result as the C++ file and network outputs write it: the msgpack array
/// `(timestamp, message, orig_file_path, archive_id, log_event_idx)`, byte-compatible with C++.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchResultTuple<'result> {
    /// Epoch milliseconds ([`crate::epoch_ns_to_wire_ms`]); 0 for a record without timestamp
    /// (DV-34).
    pub timestamp_ms: i64,

    /// The record's JSON text without a trailing newline. It is encoded like the C++ message, as
    /// the text followed by `\n`; invalid UTF-8 is replaced with U+FFFD (DV-45).
    pub message: &'result [u8],

    /// Always empty, like C++.
    pub orig_file_path: &'result str,

    pub archive_id: &'result str,

    /// The record's `log_event_idx`; 0 for an archive without log order, like C++.
    pub log_event_idx: i64,
}

impl SearchResultTuple<'_> {
    /// Appends the msgpack encoding of the tuple to `output`. On failure, `output` is left as it
    /// was.
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

    /// Appends the five fields of the tuple to `output`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`WireError::Msgpack`] if a string is longer than msgpack's `u32` length limit.
    fn encode_fields(&self, output: &mut Vec<u8>) -> Result<(), WireError> {
        const NUM_FIELDS: u32 = 5;

        rmp::encode::write_array_len(output, NUM_FIELDS).map_err(msgpack_error)?;
        rmp::encode::write_sint(output, self.timestamp_ms).map_err(msgpack_error)?;
        let message = String::from_utf8_lossy(self.message);
        let message_size = message
            .len()
            .checked_add(1)
            .ok_or_else(|| too_long("message", message.len()))?;
        write_str_header(output, "message", message_size)?;
        output.extend_from_slice(message.as_bytes());
        output.push(b'\n');
        write_str(output, "orig_file_path", self.orig_file_path)?;
        write_str(output, "archive_id", self.archive_id)?;
        rmp::encode::write_sint(output, self.log_event_idx).map_err(msgpack_error)?;
        Ok(())
    }
}

/// Appends the msgpack string `text` to `output`.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`write_str_header`]'s return values on failure.
fn write_str(output: &mut Vec<u8>, field: &str, text: &str) -> Result<(), WireError> {
    write_str_header(output, field, text.len())?;
    output.extend_from_slice(text.as_bytes());
    Ok(())
}

/// Appends the header of a msgpack string of `size` bytes to `output`.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`WireError::Msgpack`] if `size` exceeds msgpack's `u32` length limit.
fn write_str_header(output: &mut Vec<u8>, field: &str, size: usize) -> Result<(), WireError> {
    let length = u32::try_from(size).map_err(|_| too_long(field, size))?;
    rmp::encode::write_str_len(output, length).map_err(msgpack_error)?;
    Ok(())
}

/// # Returns
///
/// The error of a `field` string of `size` bytes that msgpack cannot hold.
fn too_long(field: &str, size: usize) -> WireError {
    WireError::Msgpack(
        format!("the {field} of {size} bytes exceeds the msgpack string limit").into(),
    )
}

/// # Type Parameters
///
/// * `ErrorType` - The error of the failed msgpack write.
///
/// # Returns
///
/// The wire error of a failed msgpack write.
fn msgpack_error<ErrorType: std::fmt::Display>(error: ErrorType) -> WireError {
    WireError::Msgpack(error.to_string().into())
}

#[cfg(test)]
mod tests {
    use super::SearchResultTuple;

    const BASE: SearchResultTuple<'static> = SearchResultTuple {
        timestamp_ms: 0,
        message: b"{}",
        orig_file_path: "",
        archive_id: "a.clps",
        log_event_idx: 0,
    };

    /// # Returns
    ///
    /// The encoding of `tuple`.
    fn encode(tuple: &SearchResultTuple<'_>) -> Vec<u8> {
        let mut output = Vec::new();
        tuple.encode(&mut output).expect("the tuple encodes");
        output
    }

    #[test]
    fn integers_use_the_smallest_msgpack_representation() {
        for (value, expected_prefix) in [
            (0_i64, vec![0x00]),
            (127, vec![0x7f]),
            (128, vec![0xcc, 0x80]),
            (256, vec![0xcd, 0x01, 0x00]),
            (65_536, vec![0xce, 0x00, 0x01, 0x00, 0x00]),
            (
                1_700_000_010_000,
                vec![0xcf, 0x00, 0x00, 0x01, 0x8b, 0xcf, 0xe5, 0x8f, 0x10],
            ),
            (-1, vec![0xff]),
            (-33, vec![0xd0, 0xdf]),
            (
                i64::MIN,
                vec![0xd3, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            ),
        ] {
            let encoded = encode(&SearchResultTuple {
                timestamp_ms: value,
                ..BASE
            });
            assert_eq!(encoded[0], 0x95, "{value}");
            assert_eq!(
                &encoded[1..=expected_prefix.len()],
                &expected_prefix[..],
                "{value}"
            );
        }
    }

    #[test]
    fn the_encoding_is_the_cpp_tuple_with_a_newline_terminated_message() {
        let encoded = encode(&SearchResultTuple {
            timestamp_ms: 5,
            log_event_idx: 300,
            ..BASE
        });
        let mut expected = vec![0x95, 0x05, 0xa3];
        expected.extend_from_slice(b"{}\n");
        expected.push(0xa0);
        expected.push(0xa6);
        expected.extend_from_slice(b"a.clps");
        expected.extend_from_slice(&[0xcd, 0x01, 0x2c]);
        assert_eq!(encoded, expected);
    }

    #[test]
    fn long_strings_use_wider_headers() -> anyhow::Result<()> {
        let message = vec![b'x'; 300];
        let encoded = encode(&SearchResultTuple {
            message: &message,
            ..BASE
        });
        assert_eq!(&encoded[2..5], &[0xda, 0x01, 0x2d]);
        let decoded: (i64, String, String, String, i64) = rmp_serde::from_slice(&encoded)?;
        assert_eq!(decoded.1.len(), 301);
        let message = vec![b'y'; 40];
        let encoded = encode(&SearchResultTuple {
            message: &message,
            ..BASE
        });
        assert_eq!(&encoded[2..4], &[0xd9, 41]);
        Ok(())
    }

    #[test]
    fn invalid_utf8_is_replaced_with_the_replacement_character() -> anyhow::Result<()> {
        let encoded = encode(&SearchResultTuple {
            message: b"{\"a\":\"\xff\xfe\"}",
            ..BASE
        });
        let decoded: (i64, String, String, String, i64) = rmp_serde::from_slice(&encoded)?;
        assert_eq!(decoded.1, "{\"a\":\"\u{fffd}\u{fffd}\"}\n");
        Ok(())
    }

    #[test]
    fn tuples_round_trip_and_append() -> anyhow::Result<()> {
        let mut output = vec![0xc0];
        let tuple = SearchResultTuple {
            timestamp_ms: -1_000,
            message: "{\"msg\":\"café\"}".as_bytes(),
            orig_file_path: "",
            archive_id: "logs.ts.clps",
            log_event_idx: 2990,
        };
        tuple.encode(&mut output)?;
        assert_eq!(output[0], 0xc0);
        let decoded: (i64, String, String, String, i64) = rmp_serde::from_slice(&output[1..])?;
        assert_eq!(
            decoded,
            (
                -1_000,
                "{\"msg\":\"café\"}\n".to_owned(),
                String::new(),
                "logs.ts.clps".to_owned(),
                2990
            )
        );
        Ok(())
    }
}
