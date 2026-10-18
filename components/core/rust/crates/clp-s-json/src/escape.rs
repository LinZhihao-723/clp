//! JSON string escaping, byte for byte as C++ `StringUtils::escape_json_string` and the simdjson
//! string builder that `SimdJsonStringEscaper` wraps (`clp_s/Utils.cpp`).
//!
//! The escaping is byte-local: `"` and `\` are backslash-escaped, `\b \t \n \f \r` use their short
//! forms, every other byte below 0x20 becomes `\u00xx` with lowercase hex, and every other byte
//! (DEL and non-ASCII bytes included, valid UTF-8 or not) is copied verbatim. Escaping the pieces
//! of a text therefore equals escaping the whole text.

/// Appends `text`, JSON-escaped (without surrounding quotes), to `output`.
pub fn escape_into(text: &[u8], output: &mut Vec<u8>) {
    let mut run_begin = 0;
    let mut position = 0;
    while position < text.len() {
        if let Some(chunk) = text[position..].first_chunk::<8>()
            && !word_needs_escape(u64::from_le_bytes(*chunk))
        {
            position += 8;
            continue;
        }
        let byte = text[position];
        let code = ESCAPE_CODES[usize::from(byte)];
        if 0 != code {
            output.extend_from_slice(&text[run_begin..position]);
            output.push(b'\\');
            output.push(code);
            if b'u' == code {
                output.extend_from_slice(b"00");
                output.push(LOWERCASE_HEX_DIGITS[usize::from(byte >> 4)]);
                output.push(LOWERCASE_HEX_DIGITS[usize::from(byte & 0x0f)]);
            }
            run_begin = position + 1;
        }
        position += 1;
    }
    output.extend_from_slice(&text[run_begin..]);
}

/// Per-byte escape codes: 0 copies the byte, `u` writes `\u00xx`, any other value `c` writes `\c`.
const ESCAPE_CODES: [u8; 256] = {
    let mut codes = [0_u8; 256];
    let mut byte = 0;
    while byte < 0x20 {
        codes[byte] = b'u';
        byte += 1;
    }
    codes[0x08] = b'b';
    codes[0x09] = b't';
    codes[0x0a] = b'n';
    codes[0x0c] = b'f';
    codes[0x0d] = b'r';
    codes[b'"' as usize] = b'"';
    codes[b'\\' as usize] = b'\\';
    codes
};

const LOWERCASE_HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

const REPEATED_ONES: u64 = u64::MAX / 0xff;
const REPEATED_HIGH_BITS: u64 = REPEATED_ONES * 0x80;

/// # Returns
///
/// Whether any byte of `word` needs escaping (is below 0x20, `"` or `\`).
const fn word_needs_escape(word: u64) -> bool {
    has_byte_below(word, 0x20)
        || has_byte_below(word ^ (REPEATED_ONES * b'"' as u64), 1)
        || has_byte_below(word ^ (REPEATED_ONES * b'\\' as u64), 1)
}

/// # Returns
///
/// Whether any byte of `word` is below `bound` (exact for `bound <= 0x80`).
const fn has_byte_below(word: u64, bound: u64) -> bool {
    0 != word.wrapping_sub(REPEATED_ONES * bound) & !word & REPEATED_HIGH_BITS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The escaping of C++ `StringUtils::escape_json_string`, one byte at a time.
    ///
    /// # Returns
    ///
    /// The escaped `text`.
    fn reference_escape(text: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        for byte in text.iter().copied() {
            match byte {
                b'"' => output.extend_from_slice(b"\\\""),
                b'\\' => output.extend_from_slice(b"\\\\"),
                b'\t' => output.extend_from_slice(b"\\t"),
                b'\r' => output.extend_from_slice(b"\\r"),
                b'\n' => output.extend_from_slice(b"\\n"),
                0x08 => output.extend_from_slice(b"\\b"),
                0x0c => output.extend_from_slice(b"\\f"),
                0x00..=0x1f => output.extend_from_slice(format!("\\u{byte:04x}").as_bytes()),
                _ => output.push(byte),
            }
        }
        output
    }

    /// # Returns
    ///
    /// The escaped `text`, as [`escape_into`] appends it after existing output.
    fn escape(text: &[u8]) -> Vec<u8> {
        let mut output = b"kept".to_vec();
        escape_into(text, &mut output);
        output.split_off(4)
    }

    #[test]
    fn escapes_like_cpp() {
        let cases: [(&[u8], &[u8]); 9] = [
            (b"", b""),
            (b"plain text", b"plain text"),
            (b"quote\" back\\slash", b"quote\\\" back\\\\slash"),
            (b"\x08\x09\x0a\x0c\x0d", b"\\b\\t\\n\\f\\r"),
            (b"\x00\x01\x0b\x1f", b"\\u0000\\u0001\\u000b\\u001f"),
            (b"solidus / DEL \x7f", b"solidus / DEL \x7f"),
            (
                "caf\u{e9} \u{2028}".as_bytes(),
                "caf\u{e9} \u{2028}".as_bytes(),
            ),
            (b"lone \xed\xa0\x80 \xff", b"lone \xed\xa0\x80 \xff"),
            (
                b"a long run without escapes, then one\n",
                b"a long run without escapes, then one\\n",
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(
                escape(text),
                expected,
                "{:?}",
                String::from_utf8_lossy(text)
            );
        }
    }

    #[test]
    fn every_byte_at_every_word_position_matches_the_reference() {
        for length in 1..=17 {
            for position in 0..length {
                for byte in 0..=u8::MAX {
                    let mut text = vec![b'x'; length];
                    text[position] = byte;
                    assert_eq!(
                        escape(&text),
                        reference_escape(&text),
                        "byte {byte:#04x} at {position} of {length}"
                    );
                }
            }
        }
    }

    #[test]
    fn escaping_is_byte_local() {
        let text = b"k\x01ey \"value\" with\\many\x1f escapes\n and more text after them";
        let whole = escape(text);
        for split in 0..=text.len() {
            let (left, right) = text.split_at(split);
            let mut pieces = escape(left);
            pieces.extend(escape(right));
            assert_eq!(pieces, whole, "split at {split}");
        }
    }
}
