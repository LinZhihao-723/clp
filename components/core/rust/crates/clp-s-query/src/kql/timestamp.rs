//! KQL `timestamp("...", "pattern")` literals (C++ `kql.cpp:create_timestamp_literal`), limited to
//! numeric epoch patterns.
//!
//! As in C++, both strings keep their quotes, so the quotes of a pattern match the quotes of the
//! timestamp. Supported specifiers are `\E`, `\L`, `\C` and `\N` (epoch seconds, milliseconds,
//! microseconds and nanoseconds) and `\P` (epoch time of estimated precision). Without a pattern,
//! the timestamp must be a quoted epoch integer, read with `\P` (the only C++ default pattern that
//! an integer can match). Date-time patterns are not supported yet.

use crate::QueryError;

/// Parses a timestamp literal.
///
/// # Returns
///
/// The timestamp in epoch nanoseconds on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`QueryError::InvalidTimestamp`] if the timestamp does not match the pattern, the pattern is
///   invalid or unsupported, or the value overflows.
pub fn parse_timestamp_literal(timestamp: &str, pattern: Option<&str>) -> Result<i64, QueryError> {
    let invalid = || QueryError::InvalidTimestamp {
        text: timestamp.into(),
    };
    let pattern = pattern.unwrap_or(r#""\P""#).as_bytes();
    let timestamp_bytes = timestamp.as_bytes();

    let mut epoch_ns: Option<i64> = None;
    let mut timestamp_index = 0;
    let mut pattern_index = 0;
    while let Some(&byte) = pattern.get(pattern_index) {
        pattern_index += 1;
        if b'\\' != byte {
            if timestamp_bytes.get(timestamp_index) != Some(&byte) {
                return Err(invalid());
            }
            timestamp_index += 1;
            continue;
        }
        let specifier = *pattern.get(pattern_index).ok_or_else(invalid)?;
        pattern_index += 1;
        let fixed_factor = match specifier {
            b'E' => Some(NANOSECONDS_PER_SECOND),
            b'L' => Some(NANOSECONDS_PER_MILLISECOND),
            b'C' => Some(NANOSECONDS_PER_MICROSECOND),
            b'N' => Some(1),
            b'P' => None,
            _ => return Err(invalid()),
        };
        if epoch_ns.is_some() {
            return Err(invalid());
        }
        let remaining = timestamp_bytes.get(timestamp_index..).unwrap_or_default();
        let (number, num_bytes) = parse_epoch_number(remaining).ok_or_else(invalid)?;
        timestamp_index += num_bytes;
        let factor = fixed_factor.unwrap_or_else(|| estimate_precision_factor(number));
        epoch_ns = Some(number.checked_mul(factor).ok_or_else(invalid)?);
    }
    if timestamp_index != timestamp_bytes.len() {
        return Err(invalid());
    }
    epoch_ns.ok_or_else(invalid)
}

const NANOSECONDS_PER_SECOND: i64 = 1_000_000_000;
const NANOSECONDS_PER_MILLISECOND: i64 = 1_000_000;
const NANOSECONDS_PER_MICROSECOND: i64 = 1_000;

/// Reads a decimal epoch value like C++ `convert_variable_length_string_prefix_to_number`: an
/// optional `-`, then digits without a leading zero (except a lone `0`, which cannot be negated).
///
/// # Returns
///
/// The value and the number of bytes read, or `None` if no valid number starts `text` or it
/// overflows `i64`.
fn parse_epoch_number(text: &[u8]) -> Option<(i64, usize)> {
    let is_negative = text.first() == Some(&b'-');
    let digits_start = usize::from(is_negative);
    let num_digits = text[digits_start..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if 0 == num_digits {
        return None;
    }
    let digits = &text[digits_start..digits_start + num_digits];
    if b'0' == digits[0] && (is_negative || num_digits > 1) {
        return None;
    }
    let mut value = 0_i64;
    for &digit in digits {
        let digit = i64::from(digit - b'0');
        value = value.checked_mul(10)?;
        value = if is_negative {
            value.checked_sub(digit)?
        } else {
            value.checked_add(digit)?
        };
    }
    Some((value, digits_start + num_digits))
}

/// # Returns
///
/// The nanoseconds per unit of an epoch value of unknown precision (C++
/// `estimate_timestamp_precision`): nanoseconds, microseconds, milliseconds or seconds, by
/// magnitude.
const fn estimate_precision_factor(value: i64) -> i64 {
    const EPOCH_MILLISECONDS_1971: u64 = 31_536_000_000;
    const EPOCH_MICROSECONDS_1971: u64 = 31_536_000_000_000;
    const EPOCH_NANOSECONDS_1971: u64 = 31_536_000_000_000_000;
    let magnitude = value.unsigned_abs();
    if magnitude > EPOCH_NANOSECONDS_1971 {
        1
    } else if magnitude > EPOCH_MICROSECONDS_1971 {
        NANOSECONDS_PER_MICROSECOND
    } else if magnitude > EPOCH_MILLISECONDS_1971 {
        NANOSECONDS_PER_MILLISECOND
    } else {
        NANOSECONDS_PER_SECOND
    }
}

#[cfg(test)]
mod tests {
    use super::parse_timestamp_literal;
    use crate::QueryError;

    #[test]
    fn epoch_patterns_scale_to_nanoseconds() -> anyhow::Result<()> {
        assert_eq!(parse_timestamp_literal(r#""1""#, Some(r#""\N""#))?, 1);
        assert_eq!(parse_timestamp_literal(r#""1""#, Some(r#""\C""#))?, 1_000);
        assert_eq!(
            parse_timestamp_literal(r#""1700000000123""#, Some(r#""\L""#))?,
            1_700_000_000_123_000_000
        );
        assert_eq!(
            parse_timestamp_literal(r#""-5""#, Some(r#""\E""#))?,
            -5_000_000_000
        );
        assert_eq!(parse_timestamp_literal(r#""0""#, Some(r#""\E""#))?, 0);
        assert_eq!(
            parse_timestamp_literal(r#""ts=17""#, Some(r#""ts=\E""#))?,
            17_000_000_000
        );
        Ok(())
    }

    #[test]
    fn default_pattern_estimates_the_precision() -> anyhow::Result<()> {
        assert_eq!(
            parse_timestamp_literal(r#""1700000000""#, None)?,
            1_700_000_000_000_000_000
        );
        assert_eq!(
            parse_timestamp_literal(r#""1700000000123""#, None)?,
            1_700_000_000_123_000_000
        );
        assert_eq!(
            parse_timestamp_literal(r#""1700000000123456""#, None)?,
            1_700_000_000_123_456_000
        );
        assert_eq!(
            parse_timestamp_literal(r#""1700000000123456789""#, None)?,
            1_700_000_000_123_456_789
        );
        Ok(())
    }

    #[test]
    fn invalid_or_unsupported_timestamps_are_rejected() {
        for (timestamp, pattern) in [
            (r#""a""#, None),
            (r#""12345""#, Some(r#""\Y-\m-\d""#)),
            (r#""1970-01-01 00:00:00.000000001""#, None),
            (r#""01""#, Some(r#""\E""#)),
            (r#""-0""#, Some(r#""\E""#)),
            (r#""1x""#, Some(r#""\E""#)),
            (r#""1""#, Some(r#""\E\E""#)),
            (r#""1""#, Some(r#""x""#)),
            (r#""1""#, Some(r#""\""#)),
            (r#""9223372036854775807""#, Some(r#""\E""#)),
            (r#""99999999999999999999""#, Some(r#""\N""#)),
        ] {
            assert!(
                matches!(
                    parse_timestamp_literal(timestamp, pattern),
                    Err(QueryError::InvalidTimestamp { .. })
                ),
                "{timestamp} with {pattern:?} should be rejected"
            );
        }
    }
}
