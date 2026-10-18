//! KQL literal unescaping and typing (C++ `unescape_kql_internal`, `kql.cpp:create_literal` and
//! the `istringstream` numeric rules of `Integral::create_from_string`).

use crate::PatternPiece;
use crate::QueryValue;
use crate::TextPattern;

/// A typed KQL literal.
#[derive(Debug, PartialEq)]
pub enum Literal {
    /// A literal whose whole text is the unescaped wildcard `*`.
    AnyValue,

    Value(QueryValue),
}

impl Literal {
    /// Factory function.
    ///
    /// Types an unescaped literal (the output of [`unescape_kql`] with `is_value` set) like C++:
    /// an integer that `istringstream` reads completely into an `i64`, else a finite double read
    /// completely, else `true`/`false`, else `null`, else a text pattern.
    ///
    /// # Returns
    ///
    /// The typed literal.
    pub fn from_escaped(escaped: &[u8]) -> Self {
        if let Some(text) = ascii_text(escaped) {
            if let Some(value) = parse_istream_integer(&text) {
                return Self::Value(QueryValue::Integer {
                    value,
                    text: text.into(),
                });
            }
            if let Some(value) = parse_istream_float(&text) {
                return Self::Value(QueryValue::Float {
                    value,
                    text: text.into(),
                });
            }
        }
        match escaped {
            b"true" => return Self::Value(QueryValue::Boolean(true)),
            b"false" => return Self::Value(QueryValue::Boolean(false)),
            b"null" => return Self::Value(QueryValue::Null),
            _ => {}
        }
        let pattern = text_pattern_from_escaped(escaped);
        if [PatternPiece::AnyRun] == *pattern.pieces() {
            return Self::AnyValue;
        }
        Self::Value(QueryValue::Text(pattern))
    }
}

/// Unescapes a raw KQL key or value (C++ `unescape_kql_internal`).
///
/// The output keeps the wildcard escapes of the C++ internal form: a literal backslash is `\\`, a
/// literal `*` is `\*`, and (only when `is_value` is set) a literal `?` is `\?`. Every other escape
/// is resolved: `\"`, `\t`, `\r`, `\n`, `\b`, `\f`, `\{`, `\}`, `\(`, `\)`, `\<`, `\>`, `\@`, `\$`,
/// `\!`, `\#`, and `\uXXXX` (a non-surrogate code point, written as UTF-8).
///
/// # Returns
///
/// The unescaped bytes on success.
///
/// # Errors
///
/// Returns the offset of the backslash that starts an invalid or truncated escape sequence.
pub fn unescape_kql(raw: &[u8], is_value: bool) -> Result<Vec<u8>, usize> {
    let mut unescaped = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        let byte = raw[index];
        if b'\\' != byte {
            unescaped.push(byte);
            index += 1;
            continue;
        }
        let escape_start = index;
        let Some(&escaped) = raw.get(index + 1) else {
            return Err(escape_start);
        };
        index += 2;
        match escaped {
            b'\\' => unescaped.extend_from_slice(b"\\\\"),
            b'"' => unescaped.push(b'"'),
            b't' => unescaped.push(b'\t'),
            b'r' => unescaped.push(b'\r'),
            b'n' => unescaped.push(b'\n'),
            b'b' => unescaped.push(0x08),
            b'f' => unescaped.push(0x0c),
            b'u' => {
                let code_point = raw
                    .get(index..index + 4)
                    .and_then(parse_hex_code_point)
                    .ok_or(escape_start)?;
                index += 4;
                match code_point {
                    '\\' => unescaped.extend_from_slice(b"\\\\"),
                    '?' if is_value => unescaped.extend_from_slice(b"\\?"),
                    '*' => unescaped.extend_from_slice(b"\\*"),
                    _ => {
                        let mut buffer = [0_u8; 4];
                        unescaped.extend_from_slice(code_point.encode_utf8(&mut buffer).as_bytes());
                    }
                }
            }
            b'*' => unescaped.extend_from_slice(b"\\*"),
            b'?' if is_value => unescaped.extend_from_slice(b"\\?"),
            b'{' | b'}' | b'(' | b')' | b'<' | b'>' | b'?' | b'@' | b'$' | b'!' | b'#' => {
                unescaped.push(escaped);
            }
            _ => return Err(escape_start),
        }
    }
    Ok(unescaped)
}

/// # Returns
///
/// `bytes` as a string if every byte is ASCII, else `None` (no number has a non-ASCII byte).
fn ascii_text(bytes: &[u8]) -> Option<String> {
    bytes
        .is_ascii()
        .then(|| bytes.iter().copied().map(char::from).collect())
}

/// Reads `text` as `std::istringstream(text) >> std::noskipws >> int64_t` followed by an `eof()`
/// check: an optional sign and at least one decimal digit, and nothing else, within the `i64`
/// range.
///
/// # Returns
///
/// The integer, or `None` if C++ would not type `text` as an integer.
fn parse_istream_integer(text: &str) -> Option<i64> {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Reads `text` as libstdc++'s `istringstream >> std::noskipws >> double` followed by an `eof()`
/// check.
///
/// The accepted syntax is an optional sign, digits with at most one decimal point and at least one
/// digit, and an optional exponent with at least one digit. Hexadecimal floats, `inf`, `nan` and
/// any trailing byte are rejected, and so is a value that overflows to infinity (libstdc++ sets
/// `failbit`); underflow is accepted.
///
/// # Returns
///
/// The float, or `None` if C++ would not type `text` as a float.
fn parse_istream_float(text: &str) -> Option<f64> {
    let bytes = text.as_bytes();
    let mut index = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let mut num_mantissa_digits = 0_usize;
    let mut has_decimal_point = false;
    while let Some(&byte) = bytes.get(index) {
        if byte.is_ascii_digit() {
            num_mantissa_digits += 1;
        } else if b'.' == byte && !has_decimal_point {
            has_decimal_point = true;
        } else {
            break;
        }
        index += 1;
    }
    if 0 == num_mantissa_digits {
        return None;
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let exponent_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if exponent_start == index {
            return None;
        }
    }
    if index != bytes.len() {
        return None;
    }
    text.parse::<f64>().ok().filter(|value| value.is_finite())
}

/// # Returns
///
/// The code point of four hexadecimal digits, or `None` if they are not hexadecimal or denote a
/// UTF-16 surrogate (simdjson rejects a lone surrogate, and a KQL `\u` escape never pairs).
fn parse_hex_code_point(hex: &[u8]) -> Option<char> {
    let mut value = 0_u32;
    for &digit in hex {
        value = value * 16 + char::from(digit).to_digit(16)?;
    }
    char::from_u32(value)
}

/// Converts the C++ internal escaped form into a pattern (C++ `clean_up_wildcard_search_string`
/// followed by the wildcard interpretation of the matchers): an unescaped `*` is any run, an
/// unescaped `?` is any byte, and an escaped byte is literal.
///
/// # Returns
///
/// The pattern.
fn text_pattern_from_escaped(escaped: &[u8]) -> TextPattern {
    let mut pieces = Vec::new();
    let mut literal = Vec::new();
    let mut is_escaped = false;
    for &byte in escaped {
        if is_escaped {
            is_escaped = false;
            literal.push(byte);
            continue;
        }
        let wildcard = match byte {
            b'\\' => {
                is_escaped = true;
                continue;
            }
            b'*' => PatternPiece::AnyRun,
            b'?' => PatternPiece::AnyByte,
            _ => {
                literal.push(byte);
                continue;
            }
        };
        if !literal.is_empty() {
            pieces.push(PatternPiece::Literal(
                std::mem::take(&mut literal).into_boxed_slice(),
            ));
        }
        pieces.push(wildcard);
    }
    if !literal.is_empty() {
        pieces.push(PatternPiece::Literal(literal.into_boxed_slice()));
    }
    TextPattern::new(pieces)
}

#[cfg(test)]
mod tests {
    use super::Literal;
    use super::parse_istream_float;
    use super::parse_istream_integer;
    use super::unescape_kql;
    use crate::PatternPiece;
    use crate::QueryValue;
    use crate::TextPattern;

    /// # Returns
    ///
    /// The literal that the raw KQL value `raw` spells.
    ///
    /// # Panics
    ///
    /// Panics if `raw` doesn't unescape.
    fn literal(raw: &str) -> Literal {
        let escaped = unescape_kql(raw.as_bytes(), true).expect("literal should unescape");
        Literal::from_escaped(&escaped)
    }

    /// # Returns
    ///
    /// A text literal made of `pieces`.
    fn text(pieces: Vec<PatternPiece>) -> Literal {
        Literal::Value(QueryValue::Text(TextPattern::new(pieces)))
    }

    /// # Returns
    ///
    /// A pattern piece that matches `text` literally.
    fn bytes(text: &str) -> PatternPiece {
        PatternPiece::Literal(text.as_bytes().into())
    }

    #[test]
    fn integers_follow_istringstream() {
        for (text, expected) in [
            ("5", Some(5)),
            ("+5", Some(5)),
            ("-0", Some(0)),
            ("007", Some(7)),
            ("9223372036854775807", Some(i64::MAX)),
            ("-9223372036854775808", Some(i64::MIN)),
            ("9223372036854775808", None),
            ("0x10", None),
            ("5.", None),
            (" 5", None),
            ("5 ", None),
            ("+", None),
            ("-", None),
            ("", None),
            ("+-5", None),
        ] {
            assert_eq!(parse_istream_integer(text), expected, "{text:?}");
        }
    }

    #[test]
    fn floats_follow_libstdcxx_istringstream() {
        for (text, expected) in [
            ("5.", Some(5.0)),
            (".5", Some(0.5)),
            ("+.5e-2", Some(0.005)),
            ("1e10", Some(1e10)),
            ("1E+5", Some(1e5)),
            ("0.e5", Some(0.0)),
            ("-2.25", Some(-2.25)),
            ("9223372036854775808", Some(9_223_372_036_854_775_808.0)),
            ("1e-400", Some(0.0)),
            ("1.7976931348623157e308", Some(f64::MAX)),
            ("1e400", None),
            ("-1e400", None),
            ("inf", None),
            ("nan", None),
            ("infinity", None),
            ("0x1p3", None),
            (".", None),
            ("e5", None),
            (".e5", None),
            ("5e", None),
            ("5e+", None),
            ("1.2.3", None),
            ("1e5.5", None),
            ("1,5", None),
        ] {
            assert_eq!(parse_istream_float(text), expected, "{text:?}");
        }
    }

    #[test]
    fn literals_are_typed_in_cxx_order() {
        assert_eq!(
            literal("5"),
            Literal::Value(QueryValue::Integer {
                value: 5,
                text: "5".into()
            })
        );
        assert_eq!(
            literal("3.0"),
            Literal::Value(QueryValue::Float {
                value: 3.0,
                text: "3.0".into()
            })
        );
        assert_eq!(literal("true"), Literal::Value(QueryValue::Boolean(true)));
        assert_eq!(literal("false"), Literal::Value(QueryValue::Boolean(false)));
        assert_eq!(literal("null"), Literal::Value(QueryValue::Null));
        assert_eq!(literal("TRUE"), text(vec![bytes("TRUE")]));
        assert_eq!(literal("Null"), text(vec![bytes("Null")]));
        assert_eq!(literal("0x10"), text(vec![bytes("0x10")]));
        assert_eq!(
            literal("tru\\u0065"),
            Literal::Value(QueryValue::Boolean(true))
        );
        assert_eq!(
            literal("\\u0035"),
            Literal::Value(QueryValue::Integer {
                value: 5,
                text: "5".into()
            })
        );
    }

    #[test]
    fn wildcards_become_pattern_pieces() {
        assert_eq!(literal("*"), Literal::AnyValue);
        assert_eq!(literal("**"), Literal::AnyValue);
        assert_eq!(literal("\\*"), text(vec![bytes("*")]));
        assert_eq!(
            literal("a*b?c"),
            text(vec![
                bytes("a"),
                PatternPiece::AnyRun,
                bytes("b"),
                PatternPiece::AnyByte,
                bytes("c"),
            ])
        );
        assert_eq!(literal("\\?\\\\"), text(vec![bytes("?\\")]));
        assert_eq!(literal("5*"), text(vec![bytes("5"), PatternPiece::AnyRun]));
        assert_eq!(literal(""), text(vec![]));
    }

    #[test]
    fn unescape_matches_the_cxx_tests() {
        for (raw, expected) in [
            ("\\\\", "\\\\"),
            ("\\??", "\\??"),
            ("\\**", "\\**"),
            ("\\u9999", "香"),
            ("\\r\\n\\t\\b\\f", "\r\n\t\u{8}\u{c}"),
            ("\\\"", "\""),
            ("\\{\\}\\(\\)\\<\\>", "{}()<>"),
            ("\\u003F", "\\?"),
            ("\\u002A", "\\*"),
            ("\\u005C", "\\\\"),
            ("\\@\\$\\!\\#", "@$!#"),
        ] {
            let unescaped = unescape_kql(raw.as_bytes(), true).expect("escape should be valid");
            assert_eq!(String::from_utf8_lossy(&unescaped), expected, "{raw:?}");
        }
    }

    #[test]
    fn question_marks_stay_literal_in_keys() -> anyhow::Result<()> {
        let unescaped = unescape_kql(b"\\?\\u003f", false)
            .map_err(|offset| anyhow::anyhow!("invalid escape at {offset}"))?;
        assert_eq!(unescaped, b"??");
        Ok(())
    }

    #[test]
    fn invalid_escapes_report_their_offset() {
        for (raw, offset) in [
            ("a\\", 1),
            ("ab\\z", 2),
            ("\\.", 0),
            ("\\:", 0),
            ("x\\u12", 1),
            ("\\u12G4", 0),
            ("\\ud800", 0),
            ("\\udfff", 0),
        ] {
            assert_eq!(unescape_kql(raw.as_bytes(), true), Err(offset), "{raw:?}");
        }
    }
}
