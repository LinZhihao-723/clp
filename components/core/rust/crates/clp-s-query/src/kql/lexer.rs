//! The KQL lexer: the lexer rules of `Kql.g4` with ANTLR's longest-match and rule-priority
//! resolution.

use crate::CompareOperator;
use crate::QueryError;

/// The kind of a token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    LeftParen,
    RightParen,
    LeftBrace,
    RightBrace,
    Colon,
    Comma,

    /// `timestamp(`, an implicit token of the grammar.
    TimestampOpen,

    And,
    Or,
    Not,
    Range(CompareOperator),

    /// A quoted string; the span includes both quotes.
    Quoted,

    Unquoted,
    End,
}

/// A token and its byte span in the query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub start: usize,
    pub end: usize,
}

/// Splits a query into tokens, ending with one [`TokenKind::End`] token.
///
/// # Returns
///
/// The tokens on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`QueryError::InvalidEscape`] if a backslash outside a quoted string does not start a valid
///   escape sequence.
/// * Forwards [`scan_quoted`]'s return values on failure.
pub fn tokenize(text: &str) -> Result<Vec<Token>, QueryError> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        let start = index;
        let kind = match byte {
            b' ' | b'\t' | b'\r' | b'\n' => {
                index += 1;
                continue;
            }
            b'(' => TokenKind::LeftParen,
            b')' => TokenKind::RightParen,
            b'{' => TokenKind::LeftBrace,
            b'}' => TokenKind::RightBrace,
            b':' => TokenKind::Colon,
            b'<' | b'>' => {
                let has_equal = bytes.get(index + 1) == Some(&b'=');
                index += 1 + usize::from(has_equal);
                let operator = match (byte, has_equal) {
                    (b'<', false) => CompareOperator::Less,
                    (b'<', true) => CompareOperator::LessOrEqual,
                    (_, false) => CompareOperator::Greater,
                    (_, true) => CompareOperator::GreaterOrEqual,
                };
                tokens.push(Token {
                    kind: TokenKind::Range(operator),
                    start,
                    end: index,
                });
                continue;
            }
            b'"' => {
                index = scan_quoted(bytes, start)?;
                tokens.push(Token {
                    kind: TokenKind::Quoted,
                    start,
                    end: index,
                });
                continue;
            }
            _ => {
                if bytes[start..].starts_with(TIMESTAMP_OPEN) {
                    index += TIMESTAMP_OPEN.len();
                    tokens.push(Token {
                        kind: TokenKind::TimestampOpen,
                        start,
                        end: index,
                    });
                    continue;
                }
                index = scan_unquoted(bytes, start);
                if index == start {
                    return Err(QueryError::InvalidEscape { position: start });
                }
                tokens.push(Token {
                    kind: classify_unquoted(&bytes[start..index]),
                    start,
                    end: index,
                });
                continue;
            }
        };
        index += 1;
        tokens.push(Token {
            kind,
            start,
            end: index,
        });
    }
    tokens.push(Token {
        kind: TokenKind::End,
        start: bytes.len(),
        end: bytes.len(),
    });
    Ok(tokens)
}

const TIMESTAMP_OPEN: &[u8] = b"timestamp(";

/// # Returns
///
/// The end of the quoted string starting at `start`: ANTLR's longest match of
/// `'"' ('\\"' | ~'"')* '"'`, so a quote preceded by a backslash may either continue or close the
/// string, and any other quote closes it.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`QueryError::Syntax`] if no quote can close the string.
fn scan_quoted(bytes: &[u8], start: usize) -> Result<usize, QueryError> {
    let mut last_close = None;
    for index in start + 1..bytes.len() {
        if b'"' != bytes[index] {
            continue;
        }
        last_close = Some(index);
        if index == start + 1 || b'\\' != bytes[index - 1] {
            break;
        }
    }
    last_close
        .map(|close| close + 1)
        .ok_or_else(|| QueryError::Syntax {
            position: start,
            message: "unterminated quoted string".into(),
        })
}

/// # Returns
///
/// The end of the longest unquoted literal starting at `start`: bytes other than
/// `\():<>"{}`, space, `\t`, `\r` and `\n`, plus the escapes `\t`, `\r`, `\n`, a backslash before
/// one of `\():<>"*?{}.@$!#`, and `\u` followed by four hexadecimal digits. Returns `start` if a
/// backslash at `start` does not start a valid escape.
fn scan_unquoted(bytes: &[u8], start: usize) -> usize {
    let mut index = start;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'\\' => {
                let escape_length = match bytes.get(index + 1) {
                    Some(
                        b't' | b'r' | b'n' | b'\\' | b'(' | b')' | b':' | b'<' | b'>' | b'"' | b'*'
                        | b'?' | b'{' | b'}' | b'.' | b'@' | b'$' | b'!' | b'#',
                    ) => 2,
                    Some(b'u')
                        if bytes
                            .get(index + 2..index + 6)
                            .is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit)) =>
                    {
                        6
                    }
                    _ => break,
                };
                index += escape_length;
            }
            b'(' | b')' | b':' | b'<' | b'>' | b'"' | b'{' | b'}' | b' ' | b'\r' | b'\n'
            | b'\t' => {
                break;
            }
            _ => index += 1,
        }
    }
    index
}

/// # Returns
///
/// The token kind of an unquoted literal: a keyword when the whole literal is `and`, `or` or
/// `not` in any case (keyword rules come first), `,` when the literal is a lone comma (implicit
/// tokens come first), and a literal otherwise.
fn classify_unquoted(literal: &[u8]) -> TokenKind {
    if literal.eq_ignore_ascii_case(b"and") {
        TokenKind::And
    } else if literal.eq_ignore_ascii_case(b"or") {
        TokenKind::Or
    } else if literal.eq_ignore_ascii_case(b"not") {
        TokenKind::Not
    } else if b"," == literal {
        TokenKind::Comma
    } else {
        TokenKind::Unquoted
    }
}

#[cfg(test)]
mod tests {
    use super::TokenKind;
    use super::tokenize;
    use crate::CompareOperator;
    use crate::QueryError;

    /// # Returns
    ///
    /// The kinds of the tokens of `text`.
    ///
    /// # Panics
    ///
    /// Panics if `text` doesn't tokenize.
    fn kinds(text: &str) -> Vec<TokenKind> {
        tokenize(text)
            .expect("query should tokenize")
            .into_iter()
            .map(|token| token.kind)
            .collect()
    }

    /// # Returns
    ///
    /// The source text of each token of `text`.
    ///
    /// # Panics
    ///
    /// Panics if `text` doesn't tokenize.
    fn spans(text: &str) -> Vec<&str> {
        tokenize(text)
            .expect("query should tokenize")
            .into_iter()
            .map(|token| &text[token.start..token.end])
            .collect()
    }

    #[test]
    fn punctuation_and_operators_are_tokens() {
        assert_eq!(
            kinds("a:b<=c>=(d<e>f){}"),
            [
                TokenKind::Unquoted,
                TokenKind::Colon,
                TokenKind::Unquoted,
                TokenKind::Range(CompareOperator::LessOrEqual),
                TokenKind::Unquoted,
                TokenKind::Range(CompareOperator::GreaterOrEqual),
                TokenKind::LeftParen,
                TokenKind::Unquoted,
                TokenKind::Range(CompareOperator::Less),
                TokenKind::Unquoted,
                TokenKind::Range(CompareOperator::Greater),
                TokenKind::Unquoted,
                TokenKind::RightParen,
                TokenKind::LeftBrace,
                TokenKind::RightBrace,
                TokenKind::End,
            ]
        );
    }

    #[test]
    fn keywords_are_case_insensitive_whole_literals() {
        assert_eq!(
            kinds("and AND aNd or Or not NOT andy notx"),
            [
                TokenKind::And,
                TokenKind::And,
                TokenKind::And,
                TokenKind::Or,
                TokenKind::Or,
                TokenKind::Not,
                TokenKind::Not,
                TokenKind::Unquoted,
                TokenKind::Unquoted,
                TokenKind::End,
            ]
        );
        assert_eq!(kinds(r#""and""#), [TokenKind::Quoted, TokenKind::End]);
    }

    #[test]
    fn timestamp_open_and_comma_are_implicit_tokens() {
        assert_eq!(
            kinds(r#"timestamp("1", "\N")"#),
            [
                TokenKind::TimestampOpen,
                TokenKind::Quoted,
                TokenKind::Comma,
                TokenKind::Quoted,
                TokenKind::RightParen,
                TokenKind::End,
            ]
        );
        assert_eq!(
            spans("timestamp (a,b ,"),
            ["timestamp", "(", "a,b", ",", ""]
        );
        assert_eq!(
            kinds("mytimestamp("),
            [TokenKind::Unquoted, TokenKind::LeftParen, TokenKind::End]
        );
    }

    #[test]
    fn unquoted_literals_keep_escapes_and_wildcards() {
        assert_eq!(
            spans(r"a\.b\*c?\u0041\t:x"),
            [r"a\.b\*c?\u0041\t", ":", "x", ""]
        );
        assert_eq!(spans("café=1,2"), ["café=1,2", ""]);
        assert_eq!(spans("a\u{b}b"), ["a\u{b}b", ""]);
    }

    #[test]
    fn invalid_unquoted_escapes_are_lexer_errors() {
        for (text, position) in [
            (r"a\ b", 1),
            (r"\b", 0),
            (r"ab\u12", 2),
            (r"x:\z", 2),
            ("\\", 0),
        ] {
            let result = tokenize(text);
            assert!(
                matches!(result, Err(QueryError::InvalidEscape { position: p }) if p == position),
                "{text:?} should fail at byte {position}, got {result:?}"
            );
        }
    }

    #[test]
    fn quoted_strings_take_the_longest_match() {
        assert_eq!(spans(r#""a b" c"#), [r#""a b""#, "c", ""]);
        assert_eq!(spans(r#""a\" b" c"#), [r#""a\" b""#, "c", ""]);
        assert_eq!(spans(r#""a\""#), [r#""a\""#, ""]);
        assert_eq!(spans(r#""a\\""#), [r#""a\\""#, ""]);
        assert_eq!(spans(r#""a\" b\" c"#), [r#""a\" b\""#, "c", ""]);
        assert_eq!(spans(r#""""#), [r#""""#, ""]);
        assert_eq!(spans(r#"a"b""#), ["a", r#""b""#, ""]);
        assert!(matches!(
            tokenize(r#"a: "b"#),
            Err(QueryError::Syntax { position: 3, .. })
        ));
    }

    #[test]
    fn only_four_whitespace_bytes_separate_tokens() {
        assert_eq!(spans(" a\tb\r\nc "), ["a", "b", "c", ""]);
    }
}
