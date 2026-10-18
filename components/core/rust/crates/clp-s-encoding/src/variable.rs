//! The eight-byte encodings of integer and float variables (`clp/ffi/encoding_methods.hpp`).

use crate::EncodingError;

/// An encoded variable as stored in a `ClpString` column's variable region.
pub type EncodedVariable = i64;

/// Encodes `text` as an integer variable, if the C++ encoder would.
///
/// The text must be a base-10 `i64` without a positive sign or zero padding (`-0` is not
/// encodable).
///
/// # Returns
///
/// The encoded variable, or `None` if `text` is not encodable as an integer variable.
#[must_use]
pub fn encode_integer_variable(text: &[u8]) -> Option<EncodedVariable> {
    let (&first, rest) = text.split_first()?;
    if b'-' == first {
        if !matches!(rest.first(), Some(b'1'..=b'9')) {
            return None;
        }
        return rest.iter().try_fold(0_i64, |value, &byte| {
            value
                .checked_mul(10)?
                .checked_sub(decimal_digit_value(byte)?)
        });
    }
    if !first.is_ascii_digit() || (b'0' == first && !rest.is_empty()) {
        return None;
    }
    text.iter().try_fold(0_i64, |value, &byte| {
        value
            .checked_mul(10)?
            .checked_add(decimal_digit_value(byte)?)
    })
}

/// Encodes `text` as a float variable, if the C++ encoder would.
///
/// The text must be an optional `-`, then at most 16 decimal digits with exactly one `.` that is
/// not the last byte.
///
/// # Returns
///
/// The encoded variable, or `None` if `text` is not encodable as a float variable.
#[must_use]
pub fn encode_float_variable(text: &[u8]) -> Option<EncodedVariable> {
    let (is_negative, unsigned_text) = match text.split_first() {
        None => return None,
        Some((b'-', rest)) => (true, rest),
        Some(_) => (false, text),
    };
    if unsigned_text.len() > MAX_DIGITS_IN_REPRESENTABLE_FLOAT_VARIABLE + 1 {
        return None;
    }

    let mut num_digits: u64 = 0;
    let mut digits: u64 = 0;
    let mut decimal_point_pos: Option<usize> = None;
    for (index, &byte) in unsigned_text.iter().enumerate() {
        if byte.is_ascii_digit() {
            digits = digits * 10 + u64::from(byte - b'0');
            num_digits += 1;
        } else if b'.' == byte && decimal_point_pos.is_none() {
            decimal_point_pos = Some(unsigned_text.len() - 1 - index);
        } else {
            return None;
        }
    }
    let decimal_point_pos = decimal_point_pos.filter(|&position| 0 != position)?;
    if 0 == num_digits {
        return None;
    }

    let mut encoded = u64::from(is_negative);
    encoded <<= 55;
    encoded |= digits & FLOAT_DIGITS_BIT_MASK;
    encoded <<= 4;
    encoded |= (num_digits - 1) & 0x0f;
    encoded <<= 4;
    encoded |= (decimal_point_pos as u64 - 1) & 0x0f;
    Some(encoded.cast_signed())
}

/// Appends the text of an encoded integer variable to `output`.
pub fn decode_integer_variable(encoded: EncodedVariable, output: &mut Vec<u8>) {
    output.extend_from_slice(itoa::Buffer::new().format(encoded).as_bytes());
}

/// Appends the text of an encoded float variable to `output`.
///
/// On failure, `output` is left unchanged.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`EncodingError::InvalidEncodedFloat`] if `encoded` is not a valid float encoding: its digits
///   exceed 16 decimal digits, do not fit its digit count, or its decimal point lies beyond its
///   digits.
pub fn decode_float_variable(
    encoded: EncodedVariable,
    output: &mut Vec<u8>,
) -> Result<(), EncodingError> {
    let invalid = || EncodingError::InvalidEncodedFloat {
        encoded: encoded.cast_unsigned(),
    };

    let bits = encoded.cast_unsigned();
    let decimal_point_pos = usize::from(low_nibble(bits)) + 1;
    let num_digits = usize::from(low_nibble(bits >> 4)) + 1;
    let mut digits = (bits >> 8) & FLOAT_DIGITS_BIT_MASK;
    let is_negative = 0 != bits >> 63;
    if digits > MAX_REPRESENTABLE_FLOAT_DIGITS_VALUE || num_digits < decimal_point_pos {
        return Err(invalid());
    }

    let sign_length = usize::from(is_negative);
    let value_length = num_digits + 1 + sign_length;
    let decimal_point_index = value_length - 1 - decimal_point_pos;
    let mut value = [b'0'; MAX_DECODED_FLOAT_LENGTH];
    if is_negative {
        value[0] = b'-';
    }

    // `num_unwritten` counts the unwritten bytes after the sign; the next byte written is always
    // the rightmost unwritten one, at `sign_length + num_unwritten - 1`.
    let mut num_unwritten = value_length - sign_length;
    while digits > 0 && sign_length + num_unwritten - 1 > decimal_point_index {
        value[sign_length + num_unwritten - 1] = least_significant_digit(digits);
        digits /= 10;
        num_unwritten -= 1;
    }
    if digits > 0 {
        // Skips the decimal point, which is written last.
        num_unwritten = num_unwritten.checked_sub(1).ok_or_else(invalid)?;
        while digits > 0 {
            if 0 == num_unwritten {
                return Err(invalid());
            }
            value[sign_length + num_unwritten - 1] = least_significant_digit(digits);
            digits /= 10;
            num_unwritten -= 1;
        }
    }
    value[decimal_point_index] = b'.';
    output.extend_from_slice(&value[..value_length]);
    Ok(())
}

/// Encodes a variable-dictionary id as the C++ encoder stores it (a bit cast).
///
/// # Returns
///
/// The encoded variable.
#[must_use]
pub const fn encode_dictionary_variable(id: u64) -> EncodedVariable {
    id.cast_signed()
}

/// Decodes a variable-dictionary id from an encoded variable (a bit cast).
///
/// # Returns
///
/// The variable-dictionary id.
#[must_use]
pub const fn decode_dictionary_variable(encoded: EncodedVariable) -> u64 {
    encoded.cast_unsigned()
}

const MAX_DIGITS_IN_REPRESENTABLE_FLOAT_VARIABLE: usize = 16;
const FLOAT_DIGITS_BIT_MASK: u64 = (1 << 54) - 1;
const MAX_REPRESENTABLE_FLOAT_DIGITS_VALUE: u64 = 9_999_999_999_999_999;
const MAX_DECODED_FLOAT_LENGTH: usize = MAX_DIGITS_IN_REPRESENTABLE_FLOAT_VARIABLE + 2;

/// # Returns
///
/// The value of `byte` as a decimal digit, or `None` if it is not one.
fn decimal_digit_value(byte: u8) -> Option<i64> {
    byte.is_ascii_digit().then(|| i64::from(byte - b'0'))
}

/// # Returns
///
/// The ASCII digit of the least significant decimal digit of `value`.
fn least_significant_digit(value: u64) -> u8 {
    b'0' + u8::try_from(value % 10).expect("a value modulo 10 fits in `u8`")
}

/// # Returns
///
/// The lowest four bits of `value`.
fn low_nibble(value: u64) -> u8 {
    u8::try_from(value & 0x0f).expect("a four-bit value fits in `u8`")
}

#[cfg(test)]
mod tests {
    use super::EncodedVariable;
    use super::decode_dictionary_variable;
    use super::decode_float_variable;
    use super::decode_integer_variable;
    use super::encode_dictionary_variable;
    use super::encode_float_variable;
    use super::encode_integer_variable;
    use crate::EncodingError;

    /// Builds an eight-byte float encoding from its fields, as the C++ encoder lays them out.
    ///
    /// # Returns
    ///
    /// The encoded variable.
    fn float_encoding(
        is_negative: bool,
        digits: u64,
        num_digits: u64,
        decimal_point_pos: u64,
    ) -> EncodedVariable {
        let bits = (u64::from(is_negative) << 63)
            | (digits << 8)
            | ((num_digits - 1) << 4)
            | (decimal_point_pos - 1);
        bits.cast_signed()
    }

    /// # Returns
    ///
    /// The decoded text of `encoded`.
    ///
    /// # Panics
    ///
    /// Panics if `encoded` is not a valid float encoding.
    fn decoded_float(encoded: EncodedVariable) -> String {
        let mut output = Vec::new();
        decode_float_variable(encoded, &mut output).expect("the float encoding should be valid");
        String::from_utf8(output).expect("decoded floats are ASCII")
    }

    #[test]
    fn integers_encode_like_cpp() {
        for (text, expected) in [
            ("0", Some(0)),
            ("7", Some(7)),
            ("123", Some(123)),
            ("-1", Some(-1)),
            ("-9876", Some(-9876)),
            ("9223372036854775807", Some(i64::MAX)),
            ("-9223372036854775808", Some(i64::MIN)),
            ("9223372036854775808", None),
            ("-9223372036854775809", None),
            ("", None),
            ("-", None),
            ("-0", None),
            ("00", None),
            ("01", None),
            ("-01", None),
            ("+1", None),
            ("1a", None),
            ("1.0", None),
            (" 1", None),
            ("1 ", None),
            ("0x10", None),
            ("+", None),
            ("-a", None),
            ("--", None),
            ("- 1", None),
            ("1u", None),
            ("1LL", None),
            ("0.0", None),
        ] {
            assert_eq!(
                encode_integer_variable(text.as_bytes()),
                expected,
                "text `{text}`"
            );
        }
    }

    #[test]
    fn integers_round_trip() {
        for value in [0, 1, -1, 42, -42, 1_700_000_000_000, i64::MAX, i64::MIN] {
            let text = value.to_string();
            assert_eq!(encode_integer_variable(text.as_bytes()), Some(value));
            let mut output = b"prefix:".to_vec();
            decode_integer_variable(value, &mut output);
            assert_eq!(output, format!("prefix:{text}").into_bytes());
        }
    }

    #[test]
    fn floats_encode_to_cpp_bit_layout() {
        for (text, expected) in [
            ("0.5", float_encoding(false, 5, 2, 1)),
            ("1.5", float_encoding(false, 15, 2, 1)),
            ("-1.5", float_encoding(true, 15, 2, 1)),
            (".5", float_encoding(false, 5, 1, 1)),
            ("-.5", float_encoding(true, 5, 1, 1)),
            ("0.0", float_encoding(false, 0, 2, 1)),
            ("-0.0", float_encoding(true, 0, 2, 1)),
            ("00.010", float_encoding(false, 10, 5, 3)),
            ("123.456", float_encoding(false, 123_456, 6, 3)),
            (
                "123456789012345.6",
                float_encoding(false, 1_234_567_890_123_456, 16, 1),
            ),
            (
                ".1234567890123456",
                float_encoding(false, 1_234_567_890_123_456, 16, 16),
            ),
            (
                "-.9999999999999999",
                float_encoding(true, 9_999_999_999_999_999, 16, 16),
            ),
        ] {
            assert_eq!(
                encode_float_variable(text.as_bytes()),
                Some(expected),
                "text `{text}`"
            );
        }
    }

    #[test]
    fn non_representable_floats_are_rejected() {
        for text in [
            "",
            "-",
            ".",
            "-.",
            "1",
            "1.",
            "-1.",
            "1.2.3",
            "1e5",
            "1.5e5",
            "+1.5",
            "1,5",
            "12345678901234567.0",
            "-12345678901234567.0",
            "1.23456789012345678",
            "--1.5",
            "1.-5",
            " 1.5",
            "- 1.0",
            "1.0 ",
            "1.0f",
            "1.0L",
            "18446744073709551615",
            "0000000000000000.1",
        ] {
            assert_eq!(
                encode_float_variable(text.as_bytes()),
                None,
                "text `{text}`"
            );
        }
    }

    #[test]
    fn floats_round_trip_byte_exactly() {
        for text in [
            "0.5",
            "1.5",
            "-1.5",
            ".5",
            "-.5",
            "0.0",
            "-0.0",
            "00.010",
            "000.000",
            "123.456",
            "-0.001",
            "3.14159",
            "123456789012345.6",
            ".1234567890123456",
            "-.0000000000000001",
            "-00.00",
            ".9999999999999999",
            "-.9999999999999999",
            "-25.5196868642755",
            "-999999999999999.9",
        ] {
            let encoded =
                encode_float_variable(text.as_bytes()).expect("the text should be encodable");
            assert_eq!(decoded_float(encoded), text);
        }
    }

    #[test]
    fn invalid_float_encodings_are_rejected() {
        let mut output = b"kept".to_vec();
        for encoded in [
            float_encoding(false, 10_000_000_000_000_000, 16, 1),
            float_encoding(false, 5, 2, 3),
            float_encoding(false, 123, 2, 1),
            float_encoding(false, 1_000, 3, 3),
            float_encoding(true, 100, 2, 2),
        ] {
            let result = decode_float_variable(encoded, &mut output);
            assert!(
                matches!(
                    result,
                    Err(EncodingError::InvalidEncodedFloat { encoded: reported })
                        if reported == encoded.cast_unsigned()
                ),
                "encoding {encoded:#018x}: {result:?}"
            );
        }
        assert_eq!(output, b"kept");
    }

    #[test]
    fn digits_filling_the_whole_value_decode() {
        assert_eq!(decoded_float(float_encoding(false, 123, 3, 3)), ".123");
        assert_eq!(decoded_float(float_encoding(true, 123, 3, 1)), "-12.3");
        assert_eq!(decoded_float(float_encoding(false, 0, 1, 1)), ".0");
    }

    #[test]
    fn dictionary_ids_are_bit_casts() {
        for id in [0, 1, 42, u64::MAX, 1 << 63] {
            let encoded = encode_dictionary_variable(id);
            assert_eq!(encoded.cast_unsigned(), id);
            assert_eq!(decode_dictionary_variable(encoded), id);
        }
    }
}
