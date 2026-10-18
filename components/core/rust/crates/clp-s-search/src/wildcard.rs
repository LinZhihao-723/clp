//! C++ wildcard strings (`clp/string_utils`): `*` and `?` are wildcards and `\` escapes the next
//! byte. The dictionary prefilter works on this form, as `GrepCore` does.

use clp_s_query::PatternPiece;
use clp_s_query::TextPattern;

pub const ANY_RUN: u8 = b'*';
pub const ANY_BYTE: u8 = b'?';
pub const ESCAPE: u8 = b'\\';

/// # Returns
///
/// The C++ wildcard string of `pattern`: literal `*`, `?` and `\` escaped, wildcards bare.
pub fn render(pattern: &TextPattern) -> Vec<u8> {
    let mut text = Vec::new();
    for piece in pattern.pieces() {
        match piece {
            PatternPiece::Literal(bytes) => {
                for &byte in bytes {
                    if is_wildcard(byte) || ESCAPE == byte {
                        text.push(ESCAPE);
                    }
                    text.push(byte);
                }
            }
            PatternPiece::AnyByte => text.push(ANY_BYTE),
            PatternPiece::AnyRun => text.push(ANY_RUN),
        }
    }
    text
}

/// # Returns
///
/// Whether `byte` is a wildcard.
pub const fn is_wildcard(byte: u8) -> bool {
    ANY_RUN == byte || ANY_BYTE == byte
}

/// Replaces every unescaped `from` with `to` (`string_utils::replace_unescaped_char`).
pub fn replace_unescaped(text: &mut [u8], from: u8, to: u8) {
    let mut is_escaped = false;
    for byte in text {
        if is_escaped {
            is_escaped = false;
        } else if ESCAPE == *byte {
            is_escaped = true;
        } else if from == *byte {
            *byte = to;
        }
    }
}

/// Collapses runs of `*` and drops escapes that protect neither a wildcard nor an escape
/// (`string_utils::clean_up_wildcard_search_string`).
///
/// # Returns
///
/// The cleaned string.
pub fn clean_up(text: &[u8]) -> Vec<u8> {
    let mut cleaned = Vec::with_capacity(text.len());
    let mut is_escaped = false;
    let mut position = 0;
    while position < text.len() {
        let byte = text[position];
        if is_escaped {
            is_escaped = false;
            if is_wildcard(byte) || ESCAPE == byte {
                cleaned.push(ESCAPE);
            }
            cleaned.push(byte);
            position += 1;
        } else if ANY_RUN == byte {
            cleaned.push(byte);
            position += 1;
            while position < text.len() && ANY_RUN == text[position] {
                position += 1;
            }
        } else {
            if ESCAPE == byte {
                is_escaped = true;
            } else {
                cleaned.push(byte);
            }
            position += 1;
        }
    }
    cleaned
}

/// Removes escapes (`string_utils::unescape_string`).
///
/// # Returns
///
/// The unescaped bytes.
pub fn unescape(text: &[u8]) -> Vec<u8> {
    let mut unescaped = Vec::with_capacity(text.len());
    let mut is_escaped = false;
    for &byte in text {
        if is_escaped {
            unescaped.push(byte);
            is_escaped = false;
        } else if ESCAPE == byte {
            is_escaped = true;
        } else {
            unescaped.push(byte);
        }
    }
    unescaped
}

/// Parses a wildcard string into a pattern with the matching rules of
/// `string_utils::wildcard_match_unsafe`.
///
/// A dangling final escape compares the byte after the end of the C++ string, its NUL
/// terminator, so it is kept as a literal NUL.
///
/// # Returns
///
/// The pattern.
pub fn parse(text: &[u8]) -> TextPattern {
    let mut pieces = Vec::new();
    let mut literal = Vec::new();
    let mut bytes = text.iter().copied();
    while let Some(byte) = bytes.next() {
        match byte {
            ESCAPE => literal.push(bytes.next().unwrap_or(0)),
            ANY_RUN | ANY_BYTE => {
                if !literal.is_empty() {
                    pieces.push(PatternPiece::Literal(
                        std::mem::take(&mut literal).into_boxed_slice(),
                    ));
                }
                pieces.push(if ANY_RUN == byte {
                    PatternPiece::AnyRun
                } else {
                    PatternPiece::AnyByte
                });
            }
            _ => literal.push(byte),
        }
    }
    if !literal.is_empty() {
        pieces.push(PatternPiece::Literal(literal.into_boxed_slice()));
    }
    TextPattern::new(pieces)
}

#[cfg(test)]
mod tests {
    use clp_s_query::PatternPiece;
    use clp_s_query::TextPattern;

    use super::clean_up;
    use super::parse;
    use super::render;
    use super::replace_unescaped;
    use super::unescape;

    #[test]
    fn rendering_escapes_literal_wildcards_and_escapes() {
        let pattern = TextPattern::new(vec![
            PatternPiece::AnyRun,
            PatternPiece::Literal(b"a*b?c\\d".to_vec().into_boxed_slice()),
            PatternPiece::AnyByte,
        ]);
        assert_eq!(render(&pattern), br"*a\*b\?c\\d?".to_vec());
        assert_eq!(parse(&render(&pattern)), pattern);
    }

    #[test]
    fn clean_up_matches_the_cpp_rules() {
        assert_eq!(clean_up(b"a***b"), b"a*b".to_vec());
        assert_eq!(clean_up(br"a\b\*\\"), br"ab\*\\".to_vec());
        assert_eq!(clean_up(br"\**"), br"\**".to_vec());
        assert_eq!(clean_up(b"?*"), b"?*".to_vec());
    }

    #[test]
    fn unescaped_replacement_skips_escaped_bytes() {
        let mut text = br"a?b\?c?".to_vec();
        replace_unescaped(&mut text, b'?', b'*');
        assert_eq!(text, br"a*b\?c*".to_vec());
        assert_eq!(unescape(br"a\*b\\c\"), br"a*b\c".to_vec());
    }

    #[test]
    fn parsed_patterns_match_like_wildcard_match_unsafe() {
        for (wild, tame, is_case_sensitive, expected) in [
            (&b"*error*"[..], &b"an error here"[..], true, true),
            (b"*ERROR*", b"an error here", false, true),
            (b"*ERROR*", b"an error here", true, false),
            (br"a\*b", b"a*b", true, true),
            (br"a\*b", b"axb", true, false),
            (b"a?c", b"abc", true, true),
            (br"a\\", b"a\\", true, true),
            (br"a\", b"a\\", true, false),
            (br"a\", b"a\0", true, true),
        ] {
            assert_eq!(
                parse(wild).matches(tame, is_case_sensitive),
                expected,
                "{:?} vs {:?}",
                String::from_utf8_lossy(wild),
                String::from_utf8_lossy(tame)
            );
        }
    }
}
