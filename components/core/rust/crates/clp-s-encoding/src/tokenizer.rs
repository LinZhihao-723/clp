//! The potential-variable tokenizer of the C++ encoder (`clp/ir/parsing.cpp`,
//! `clp/EncodedVariableInterpreter.hpp`), as used to encode search strings.

use std::ops::Range;

use crate::encode_float_variable;
use crate::encode_integer_variable;

/// How the encoder would store a variable token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VariableTokenKind {
    /// Encodable as an integer variable.
    Integer,

    /// Encodable as a float variable.
    Float,

    /// Stored in the variable dictionary.
    Dictionary,
}

impl VariableTokenKind {
    /// Classifies a token the way `EncodedVariableInterpreter::encode_var` does.
    ///
    /// # Returns
    ///
    /// The kind of `token`.
    #[must_use]
    pub fn of_token(token: &[u8]) -> Self {
        if encode_integer_variable(token).is_some() {
            Self::Integer
        } else if encode_float_variable(token).is_some() {
            Self::Float
        } else {
            Self::Dictionary
        }
    }
}

/// One potential variable found in a text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VariableToken {
    /// Byte range of the token in the tokenized text.
    pub range: Range<usize>,

    /// How the encoder would store the token.
    pub kind: VariableTokenKind,
}

/// Iterates over the potential variables of a text, in order, exactly as the C++ encoder splits a
/// message into a logtype and variables.
///
/// A token is a maximal run of non-delimiters (`+`, `-`, `.`, digits, ASCII letters, `\` and `_`;
/// every other byte, including every non-ASCII byte, is a delimiter). A token is a variable if it
/// contains a decimal digit, if it directly follows `=` and contains a letter, or if it is a hex
/// value of at least two digits.
#[derive(Debug)]
pub struct VariableTokenizer<'text> {
    text: &'text [u8],
    position: usize,
}

impl<'text> VariableTokenizer<'text> {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A tokenizer positioned at the start of `text`.
    #[must_use]
    pub const fn new(text: &'text [u8]) -> Self {
        Self { text, position: 0 }
    }

    /// # Returns
    ///
    /// The text being tokenized.
    #[must_use]
    pub const fn text(&self) -> &'text [u8] {
        self.text
    }
}

impl Iterator for VariableTokenizer<'_> {
    type Item = VariableToken;

    fn next(&mut self) -> Option<Self::Item> {
        let text = self.text;
        while self.position < text.len() {
            let Some(begin) = text[self.position..]
                .iter()
                .position(|&byte| !is_delimiter(byte))
                .map(|offset| self.position + offset)
            else {
                self.position = text.len();
                return None;
            };

            let mut contains_decimal_digit = false;
            let mut contains_alphabet = false;
            let mut end = begin;
            while end < text.len() {
                let byte = text[end];
                if byte.is_ascii_digit() {
                    contains_decimal_digit = true;
                } else if byte.is_ascii_alphabetic() {
                    contains_alphabet = true;
                } else if is_delimiter(byte) {
                    break;
                }
                end += 1;
            }
            self.position = end;

            let token = &text[begin..end];
            if contains_decimal_digit
                || (contains_alphabet && begin > 0 && b'=' == text[begin - 1])
                || could_be_multi_digit_hex_value(token)
            {
                return Some(VariableToken {
                    range: begin..end,
                    kind: VariableTokenKind::of_token(token),
                });
            }
        }
        None
    }
}

/// Mirrors `clp::ir::is_delim`, whose `signed char` argument makes every non-ASCII byte a
/// delimiter.
///
/// # Returns
///
/// Whether `byte` is a delimiter.
const fn is_delimiter(byte: u8) -> bool {
    !matches!(byte, b'+' | b'-' | b'.' | b'0'..=b'9' | b'A'..=b'Z' | b'\\' | b'_' | b'a'..=b'z')
}

/// # Returns
///
/// Whether `token` has at least two bytes, all of them hex digits.
fn could_be_multi_digit_hex_value(token: &[u8]) -> bool {
    token.len() >= 2 && token.iter().all(u8::is_ascii_hexdigit)
}

#[cfg(test)]
mod tests {
    use super::VariableToken;
    use super::VariableTokenKind;
    use super::VariableTokenizer;

    /// # Returns
    ///
    /// The `(token text, kind)` pairs of `text`.
    fn tokens(text: &str) -> Vec<(String, VariableTokenKind)> {
        VariableTokenizer::new(text.as_bytes())
            .map(|VariableToken { range, kind }| {
                (
                    String::from_utf8_lossy(&text.as_bytes()[range]).into_owned(),
                    kind,
                )
            })
            .collect()
    }

    /// # Returns
    ///
    /// `pairs` with owned token texts.
    fn expected(pairs: &[(&str, VariableTokenKind)]) -> Vec<(String, VariableTokenKind)> {
        pairs
            .iter()
            .map(|&(token, kind)| (token.to_owned(), kind))
            .collect()
    }

    #[test]
    fn tokens_match_cpp_variable_schemas() {
        use VariableTokenKind::Dictionary;
        use VariableTokenKind::Float;
        use VariableTokenKind::Integer;

        for (text, pairs) in [
            ("", &[][..]),
            ("no variables here", &[]),
            ("took 123 ms", &[("123", Integer)]),
            ("value -5 and 3.25", &[("-5", Integer), ("3.25", Float)]),
            (
                "user=alice id=bob7",
                &[("alice", Dictionary), ("bob7", Dictionary)],
            ),
            ("=alice", &[("alice", Dictionary)]),
            ("x= alice", &[]),
            ("addr 0xdeadbeef", &[("0xdeadbeef", Dictionary)]),
            ("hex ab cd e", &[("ab", Dictionary), ("cd", Dictionary)]),
            (
                "deadbeef cafe",
                &[("deadbeef", Dictionary), ("cafe", Dictionary)],
            ),
            (
                "v1.2.3 build_42",
                &[("v1.2.3", Dictionary), ("build_42", Dictionary)],
            ),
            ("path\\to\\file9", &[("path\\to\\file9", Dictionary)]),
            (
                "+1 -0 007",
                &[("+1", Dictionary), ("-0", Dictionary), ("007", Dictionary)],
            ),
            (
                "a=1,b=2;c:3",
                &[("1", Integer), ("2", Integer), ("3", Integer)],
            ),
            ("1.", &[("1.", Dictionary)]),
            (".5 -.5", &[(".5", Float), ("-.5", Float)]),
            (
                "9223372036854775808",
                &[("9223372036854775808", Dictionary)],
            ),
            (
                "12345678901234567.0",
                &[("12345678901234567.0", Dictionary)],
            ),
            ("caf\u{e9}1 x", &[("caf", Dictionary), ("1", Integer)]),
            ("tab\t7\n8", &[("7", Integer), ("8", Integer)]),
        ] {
            assert_eq!(tokens(text), expected(pairs), "text {text:?}");
        }
    }

    // The strings of `components/core/tests/test-ir_parsing.cpp`.
    #[test]
    fn cpp_parsing_cases_tokenize_identically() {
        for (text, expected_tokens) in [
            ("", &[][..]),
            ("/", &[]),
            ("xyz", &[]),
            ("=", &[]),
            ("~=x!abc123;1.2%x:+394/-", &["x", "abc123", "1.2", "+394"]),
            (" ad ff 95 24 0d ff ", &["ad", "ff", "95", "24", "0d", "ff"]),
            (" text \x11 var123 ", &["var123"]),
        ] {
            let found: Vec<_> = tokens(text).into_iter().map(|(token, _)| token).collect();
            assert_eq!(found, expected_tokens, "text {text:?}");
        }
    }

    #[test]
    fn ranges_index_the_original_text() {
        let text = b"a=1 bb 22";
        let ranges: Vec<_> = VariableTokenizer::new(text)
            .map(|token| token.range)
            .collect();
        assert_eq!(ranges, vec![2..3, 4..6, 7..9]);
    }

    #[test]
    fn exhausted_tokenizer_stays_exhausted() {
        let mut tokenizer = VariableTokenizer::new(b"1 2");
        assert_eq!(tokenizer.by_ref().count(), 2);
        assert_eq!(tokenizer.next(), None);
    }
}
