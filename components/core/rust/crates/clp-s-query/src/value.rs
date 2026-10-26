//! Typed literals, text patterns and search types.

use crate::CompareOperator;

/// A literal as typed by KQL (search-type layer 1).
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq)]
pub enum QueryValue {
    /// An integer literal, with its original text (an integer also compares with text equal to
    /// it).
    Integer {
        value: i64,
        text: Box<str>,
    },

    /// A float literal, with its original text.
    Float {
        value: f64,
        text: Box<str>,
    },

    Boolean(bool),
    Null,
    Text(TextPattern),

    /// A timestamp literal, or a time bound.
    Timestamp {
        epoch_ns: i64,
    },
}

impl QueryValue {
    /// # Returns
    ///
    /// The search types this value is comparable with under `operator` (design report §8.1 table).
    #[must_use]
    pub fn comparable_search_types(&self, operator: CompareOperator) -> SearchTypeSet {
        let is_equal = CompareOperator::Equal == operator;
        let numeric = SearchTypeSet::EMPTY
            .with(SearchType::Integer)
            .with(SearchType::Float)
            .with(SearchType::Timestamp)
            .with(SearchType::Array);
        match self {
            Self::Integer { .. } | Self::Float { .. } if is_equal => numeric.with(SearchType::Text),
            Self::Integer { .. } | Self::Float { .. } | Self::Timestamp { .. } => numeric,
            Self::Boolean(_) if is_equal => SearchTypeSet::EMPTY
                .with(SearchType::Boolean)
                .with(SearchType::Text)
                .with(SearchType::Array),
            Self::Null if is_equal => numeric
                .with(SearchType::Text)
                .with(SearchType::Boolean)
                .with(SearchType::Null),
            Self::Text(_) if is_equal => SearchTypeSet::EMPTY
                .with(SearchType::Text)
                .with(SearchType::Array),
            Self::Boolean(_) | Self::Null | Self::Text(_) => SearchTypeSet::EMPTY,
        }
    }
}

/// A whole-value byte glob with escapes resolved: `*` is any run, `?` is any one byte.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TextPattern {
    pieces: Box<[PatternPiece]>,
}

impl TextPattern {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The pattern made of `pieces`, with adjacent literals merged and runs of `*` collapsed.
    #[must_use]
    pub fn new(pieces: Vec<PatternPiece>) -> Self {
        let mut normalized: Vec<PatternPiece> = Vec::with_capacity(pieces.len());
        for piece in pieces {
            match (normalized.last_mut(), piece) {
                (_, PatternPiece::Literal(bytes)) if bytes.is_empty() => {}
                (Some(PatternPiece::Literal(previous)), PatternPiece::Literal(bytes)) => {
                    let mut merged = std::mem::take(previous).into_vec();
                    merged.extend_from_slice(&bytes);
                    *previous = merged.into_boxed_slice();
                }
                (Some(PatternPiece::AnyRun), PatternPiece::AnyRun) => {}
                (_, piece) => normalized.push(piece),
            }
        }
        Self {
            pieces: normalized.into_boxed_slice(),
        }
    }

    /// # Returns
    ///
    /// The pieces of the pattern.
    #[must_use]
    pub fn pieces(&self) -> &[PatternPiece] {
        &self.pieces
    }

    /// # Returns
    ///
    /// The literal bytes if the pattern has no wildcard, else `None`.
    #[must_use]
    pub fn as_literal(&self) -> Option<&[u8]> {
        match &*self.pieces {
            [] => Some(&[]),
            [PatternPiece::Literal(bytes)] => Some(bytes),
            _ => None,
        }
    }

    /// # Returns
    ///
    /// Whether the pattern contains a wildcard.
    #[must_use]
    pub fn has_wildcards(&self) -> bool {
        self.pieces
            .iter()
            .any(|piece| !matches!(piece, PatternPiece::Literal(_)))
    }

    /// # Returns
    ///
    /// Whether `text` matches the whole pattern; ASCII case is folded unless `is_case_sensitive`.
    #[must_use]
    pub fn matches(&self, text: &[u8], is_case_sensitive: bool) -> bool {
        match &*self.pieces {
            [PatternPiece::Literal(prefix), PatternPiece::AnyRun] => {
                return text
                    .get(..prefix.len())
                    .is_some_and(|head| bytes_equal(head, prefix, is_case_sensitive));
            }
            [PatternPiece::AnyRun, PatternPiece::Literal(suffix)] => {
                return text
                    .len()
                    .checked_sub(suffix.len())
                    .is_some_and(|start| bytes_equal(&text[start..], suffix, is_case_sensitive));
            }
            _ => {}
        }
        let mut segments = self.pieces.split(|piece| PatternPiece::AnyRun == *piece);
        let first = segments.next().unwrap_or_default();
        let Some((head, mut remaining)) = text.split_at_checked(segment_length(first)) else {
            return false;
        };
        if !segment_matches(first, head, is_case_sensitive) {
            return false;
        }
        let Some(last) = segments.next_back() else {
            return remaining.is_empty();
        };

        // Between two stars, taking the leftmost match of a fixed-length segment never loses a
        // match, so no backtracking is needed.
        for segment in segments {
            let length = segment_length(segment);
            let Some(max_start) = remaining.len().checked_sub(length) else {
                return false;
            };
            let Some(start) = (0..=max_start).find(|&start| {
                segment_matches(
                    segment,
                    &remaining[start..start + length],
                    is_case_sensitive,
                )
            }) else {
                return false;
            };
            remaining = &remaining[start + length..];
        }

        let Some(tail_start) = remaining.len().checked_sub(segment_length(last)) else {
            return false;
        };
        segment_matches(last, &remaining[tail_start..], is_case_sensitive)
    }
}

/// One piece of a [`TextPattern`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PatternPiece {
    Literal(Box<[u8]>),

    /// `?`.
    AnyByte,

    /// `*`.
    AnyRun,
}

/// The domain a comparison is defined over (search-type layer 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SearchType {
    Integer,
    Float,
    Text,
    Boolean,
    Null,
    Timestamp,
    Array,
}

/// A set of [`SearchType`]s.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SearchTypeSet(u8);

impl SearchTypeSet {
    /// The set of every search type.
    pub const ALL: Self = Self::EMPTY
        .with(SearchType::Integer)
        .with(SearchType::Float)
        .with(SearchType::Text)
        .with(SearchType::Boolean)
        .with(SearchType::Null)
        .with(SearchType::Timestamp)
        .with(SearchType::Array);
    /// The empty set.
    pub const EMPTY: Self = Self(0);

    /// # Returns
    ///
    /// The set with `search_type` added.
    #[must_use]
    pub const fn with(self, search_type: SearchType) -> Self {
        Self(self.0 | (1 << search_type as u8))
    }

    /// # Returns
    ///
    /// The union of both sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// # Returns
    ///
    /// Whether `search_type` is in the set.
    #[must_use]
    pub const fn contains(self, search_type: SearchType) -> bool {
        0 != self.0 & (1 << search_type as u8)
    }

    /// # Returns
    ///
    /// Whether the set is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        0 == self.0
    }
}

/// # Returns
///
/// Whether `lhs` equals `rhs`; ASCII case is folded unless `is_case_sensitive`.
fn bytes_equal(lhs: &[u8], rhs: &[u8], is_case_sensitive: bool) -> bool {
    if is_case_sensitive {
        lhs == rhs
    } else {
        lhs.eq_ignore_ascii_case(rhs)
    }
}

/// # Returns
///
/// The number of text bytes a star-free segment matches.
fn segment_length(segment: &[PatternPiece]) -> usize {
    segment
        .iter()
        .map(|piece| match piece {
            PatternPiece::Literal(bytes) => bytes.len(),
            PatternPiece::AnyByte | PatternPiece::AnyRun => 1,
        })
        .sum()
}

/// # Returns
///
/// Whether `text`, of the segment's length, matches the star-free `segment`.
fn segment_matches(segment: &[PatternPiece], text: &[u8], is_case_sensitive: bool) -> bool {
    let mut offset = 0;
    for piece in segment {
        match piece {
            PatternPiece::Literal(bytes) => {
                let Some(window) = text.get(offset..offset + bytes.len()) else {
                    return false;
                };
                if !bytes_equal(window, bytes, is_case_sensitive) {
                    return false;
                }
                offset += bytes.len();
            }
            PatternPiece::AnyByte | PatternPiece::AnyRun => offset += 1,
        }
    }
    offset == text.len()
}

#[cfg(test)]
mod tests {
    use super::PatternPiece;
    use super::QueryValue;
    use super::SearchType;
    use super::SearchTypeSet;
    use super::TextPattern;
    use crate::CompareOperator;

    /// # Returns
    ///
    /// The text pattern of the glob `glob`, where `*` and `?` are wildcards.
    fn pattern(glob: &str) -> TextPattern {
        let mut pieces = Vec::new();
        for byte in glob.bytes() {
            pieces.push(match byte {
                b'*' => PatternPiece::AnyRun,
                b'?' => PatternPiece::AnyByte,
                _ => PatternPiece::Literal(Box::new([byte])),
            });
        }
        TextPattern::new(pieces)
    }

    #[test]
    fn construction_merges_literals_and_collapses_stars() {
        assert_eq!(
            pattern("ab**c?").pieces(),
            [
                PatternPiece::Literal(b"ab".to_vec().into_boxed_slice()),
                PatternPiece::AnyRun,
                PatternPiece::Literal(b"c".to_vec().into_boxed_slice()),
                PatternPiece::AnyByte,
            ]
        );
        assert_eq!(pattern("abc").as_literal(), Some(&b"abc"[..]));
        assert_eq!(pattern("").as_literal(), Some(&b""[..]));
        assert_eq!(pattern("a*").as_literal(), None);
        assert!(pattern("a?").has_wildcards());
        assert!(!pattern("a").has_wildcards());
    }

    #[test]
    fn globs_match_whole_values() {
        for (glob, text, expected) in [
            ("abc", "abc", true),
            ("abc", "abcd", false),
            ("", "", true),
            ("", "a", false),
            ("*", "", true),
            ("*", "anything", true),
            ("a*", "a", true),
            ("a*", "ba", false),
            ("*a", "ba", true),
            ("*a", "ab", false),
            ("a*b*c", "abc", true),
            ("a*b*c", "aXbYc", true),
            ("a*b*c", "aXcYb", false),
            ("*ab*ab*", "xabyab", true),
            ("*ab*ab*", "xaby", false),
            ("a?c", "abc", true),
            ("a?c", "ac", false),
            ("?", "", false),
            ("*?*", "x", true),
            ("*aab", "aaab", true),
            ("*failed*", "request 7 failed with status 500", true),
        ] {
            assert_eq!(
                pattern(glob).matches(text.as_bytes(), true),
                expected,
                "{glob:?} vs {text:?}"
            );
        }
    }

    #[test]
    fn wildcards_count_bytes_not_characters() {
        assert!(pattern("caf??").matches("café".as_bytes(), true));
        assert!(!pattern("caf?").matches("café".as_bytes(), true));
    }

    #[test]
    fn case_folding_is_ascii_only() {
        assert!(pattern("ERROR").matches(b"error", false));
        assert!(!pattern("ERROR").matches(b"error", true));
        assert!(pattern("e*R").matches(b"ExxR", false));
        assert!(pattern("ERR*").matches(b"error", false));
        assert!(!pattern("ERR*").matches(b"error", true));
        assert!(pattern("*OR").matches(b"error", false));
        assert!(!pattern("*OR").matches(b"error", true));
        assert!(!pattern("É").matches("é".as_bytes(), false));
    }

    #[test]
    fn comparable_search_types_follow_the_coercion_table() {
        let set = |types: &[SearchType]| {
            types
                .iter()
                .fold(SearchTypeSet::EMPTY, |set, &search_type| {
                    set.with(search_type)
                })
        };
        let numeric = [
            SearchType::Integer,
            SearchType::Float,
            SearchType::Timestamp,
            SearchType::Array,
        ];
        let integer = QueryValue::Integer {
            value: 5,
            text: "5".into(),
        };
        let float = QueryValue::Float {
            value: 0.5,
            text: "0.5".into(),
        };
        let text = QueryValue::Text(pattern("a"));
        let timestamp = QueryValue::Timestamp { epoch_ns: 0 };
        for value in [&integer, &float] {
            assert_eq!(
                value.comparable_search_types(CompareOperator::Equal),
                set(&numeric).with(SearchType::Text)
            );
            assert_eq!(
                value.comparable_search_types(CompareOperator::Less),
                set(&numeric)
            );
        }
        for operator in [CompareOperator::Equal, CompareOperator::GreaterOrEqual] {
            assert_eq!(timestamp.comparable_search_types(operator), set(&numeric));
        }
        assert_eq!(
            QueryValue::Boolean(true).comparable_search_types(CompareOperator::Equal),
            set(&[SearchType::Boolean, SearchType::Text, SearchType::Array])
        );
        assert_eq!(
            QueryValue::Null.comparable_search_types(CompareOperator::Equal),
            SearchTypeSet::ALL
        );
        assert_eq!(
            text.comparable_search_types(CompareOperator::Equal),
            set(&[SearchType::Text, SearchType::Array])
        );
        for value in [QueryValue::Boolean(true), QueryValue::Null, text] {
            for operator in [
                CompareOperator::Less,
                CompareOperator::LessOrEqual,
                CompareOperator::Greater,
                CompareOperator::GreaterOrEqual,
            ] {
                assert_eq!(
                    value.comparable_search_types(operator),
                    SearchTypeSet::EMPTY
                );
            }
        }
    }
}
