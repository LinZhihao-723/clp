//! The `ClpString` dictionary prefilter: a port of the heuristic path of
//! `clp::GrepCore::process_raw_query` (`clp/GrepCore.hpp`, `clp/GrepCore.cpp`,
//! `clp/QueryToken.cpp`), the only path clp-s uses, with fixes that make it never reject a value
//! the pattern matches (DV-17):
//!
//! * an unescaped `?` in the pattern makes every subquery require the glob check, since the
//!   subqueries are generated with `?` widened to `*`;
//! * a variable token with an inner `*` matches any span of the logtype, since the `*` can cross
//!   delimiters (C++ requires one dictionary placeholder, which misses integer and float variables
//!   and spans of several message tokens);
//! * a variable token with a leading or trailing `*` whose text could be part of a number is also
//!   tried as an integer and a float variable (C++ tries those only if the text without the `*` is
//!   itself encodable, which misses `*0.*` on `0.041`);
//! * constants are translated into logtype globs byte by byte (C++ drops the escape of a literal
//!   backslash that precedes a wildcard or ends a constant, and of a placeholder byte that precedes
//!   a non-wildcard, so such patterns match no logtype).

use clp_s_encoding::VariablePlaceholder;
use clp_s_encoding::encode_float_variable;
use clp_s_encoding::encode_integer_variable;
use clp_s_format::LogTypeDictionary;
use clp_s_format::VariableDictionary;
use clp_s_query::PatternPiece;
use clp_s_query::TextPattern;

use crate::IdSet;
use crate::wildcard;

/// The prefilter of one text pattern against an archive's logtype and variable dictionaries.
///
/// The row-level contract: a `ClpString` value is False if its logtype and variables match no
/// subquery. A value matching a subquery that needs no glob check is True. Any other matching
/// value is True iff its decoded text matches the pattern. With [`Self::DecodeAll`], every value
/// is decoded and matched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EncodedTextMatch {
    /// No value of the archive can match (the C++ `std::nullopt`).
    Never,

    /// The logtype pattern is `*`: decode and match every value.
    DecodeAll,

    /// The candidate interpretations of the pattern; never empty.
    Subqueries(Box<[EncodedSubquery]>),
}

impl EncodedTextMatch {
    /// The most interpretations of a pattern that are enumerated; C++ enumerates every one, which
    /// is exponential in the number of ambiguous tokens. Beyond the cap the prefilter decodes
    /// every value, which selects the same values.
    pub const MAX_INTERPRETATIONS: u64 = 1024;

    /// Factory function.
    ///
    /// Runs the `GrepCore` heuristic for `pattern` (with `?` and `*` wildcards) against the
    /// dictionaries, or decodes every value if the pattern has more than
    /// [`Self::MAX_INTERPRETATIONS`] interpretations.
    ///
    /// # Returns
    ///
    /// The prefilter of `pattern`.
    #[must_use]
    pub fn new(
        pattern: &TextPattern,
        is_case_sensitive: bool,
        logtypes: &LogTypeDictionary,
        variables: &VariableDictionary,
    ) -> Self {
        let has_single_byte_wildcard = pattern.pieces().contains(&PatternPiece::AnyByte);
        let mut search_string = wildcard::render(pattern);
        wildcard::replace_unescaped(&mut search_string, wildcard::ANY_BYTE, wildcard::ANY_RUN);
        let search_string = wildcard::clean_up(&search_string);

        let mut tokens = Vec::new();
        let mut bounds = TokenBounds::default();
        while bounds.advance(&search_string) {
            tokens.push(QueryToken::new(
                &search_string,
                bounds.begin,
                bounds.end,
                bounds.is_var,
            ));
        }
        let ambiguous_tokens: Vec<usize> = tokens
            .iter()
            .enumerate()
            .filter(|(_, token)| token.num_interpretations() > 1)
            .map(|(index, _)| index)
            .collect();
        let num_interpretations = ambiguous_tokens.iter().try_fold(1_u64, |product, index| {
            product.checked_mul(tokens[*index].num_interpretations() as u64)
        });
        if num_interpretations.is_none_or(|count| count > Self::MAX_INTERPRETATIONS) {
            return Self::DecodeAll;
        }

        let generator = SubqueryGenerator {
            search_string: &search_string,
            is_case_sensitive,
            logtypes,
            variables,
        };
        let mut subqueries = Vec::new();
        loop {
            match generator.generate(&tokens) {
                Matchability::SupersedesAll => return Self::DecodeAll,
                Matchability::MayMatch(mut subquery) => {
                    subquery.is_glob_check_required |= has_single_byte_wildcard;
                    if !subqueries.contains(&subquery) {
                        subqueries.push(subquery);
                    }
                }
                Matchability::WontMatch => {}
            }
            let is_changed = ambiguous_tokens
                .iter()
                .any(|index| tokens[*index].change_to_next_interpretation());
            if !is_changed {
                break;
            }
        }
        if subqueries.is_empty() {
            Self::Never
        } else {
            Self::Subqueries(subqueries.into_boxed_slice())
        }
    }

    /// # Returns
    ///
    /// The heap bytes the prefilter holds (for plan limits).
    #[must_use]
    pub fn heap_size(&self) -> u64 {
        match self {
            Self::Never | Self::DecodeAll => 0,
            Self::Subqueries(subqueries) => subqueries
                .iter()
                .map(|subquery| {
                    std::mem::size_of::<EncodedSubquery>() as u64
                        + subquery.logtypes.heap_size()
                        + subquery
                            .variables
                            .iter()
                            .map(|variable| match variable {
                                EncodedVariableTest::Exact(_) => 8,
                                EncodedVariableTest::DictionaryIds(ids) => 8 + ids.heap_size(),
                            })
                            .sum::<u64>()
                })
                .sum(),
        }
    }
}

/// One interpretation of a pattern (C++ `SubQuery`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedSubquery {
    /// The logtype ids the interpretation matches.
    pub logtypes: IdSet,

    /// Tests the value's encoded variables must pass in order, not necessarily contiguously (C++
    /// `SubQuery::matches_vars`).
    pub variables: Box<[EncodedVariableTest]>,

    /// Whether a value matching the logtypes and variables must still be decoded and matched.
    pub is_glob_check_required: bool,
}

/// A test of one encoded variable (C++ `QueryVar`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EncodedVariableTest {
    /// The encoded variable equals this value (an integer or float encoding).
    Exact(i64),

    /// The encoded variable, read as a dictionary id, is one of these.
    DictionaryIds(IdSet),
}

impl EncodedVariableTest {
    /// # Returns
    ///
    /// Whether the encoded variable `encoded` passes the test.
    #[must_use]
    pub fn matches(&self, encoded: i64) -> bool {
        match self {
            Self::Exact(value) => *value == encoded,
            Self::DictionaryIds(ids) => ids.contains(encoded.cast_unsigned()),
        }
    }
}

/// The outcome of generating one subquery.
enum Matchability {
    MayMatch(EncodedSubquery),
    WontMatch,
    SupersedesAll,
}

/// The types a token is tried as (C++ `QueryToken::Type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokenType {
    Wildcard,
    Logtype,
    DictionaryVar,
    FloatVar,
    IntVar,
}

/// One token of the search string (C++ `QueryToken`).
struct QueryToken {
    begin: usize,
    end: usize,
    value: Vec<u8>,
    cannot_convert_to_non_dict_var: bool,
    wildcards: GreedyWildcards,

    /// The types the token is tried as; one entry unless the token is ambiguous.
    types: Vec<TokenType>,
    current_type: usize,
}

impl QueryToken {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The token `search_string[begin..end]`, typed as C++ types it.
    fn new(search_string: &[u8], begin: usize, end: usize, is_var: bool) -> Self {
        let value = search_string[begin..end].to_vec();
        let mut token = Self {
            begin,
            end,
            value,
            cannot_convert_to_non_dict_var: false,
            wildcards: GreedyWildcards::default(),
            types: Vec::new(),
            current_type: 0,
        };
        if b"*" == token.value.as_slice() {
            token.wildcards.is_prefix = true;
            token.types.push(TokenType::Wildcard);
            return token;
        }

        let value = &token.value;
        token.wildcards = GreedyWildcards {
            is_prefix: value.first() == Some(&wildcard::ANY_RUN),
            is_suffix: value.last() == Some(&wildcard::ANY_RUN),
            is_in_middle: value.len() > 2 && value[1..value.len() - 1].contains(&wildcard::ANY_RUN),
        };

        if is_var {
            let mut without_wildcards: &[u8] = value;
            if token.wildcards.is_prefix {
                without_wildcards = &without_wildcards[1..];
            }
            if token.wildcards.is_suffix {
                without_wildcards = without_wildcards
                    .split_last()
                    .map_or(without_wildcards, |(_, rest)| rest);
            }
            let converts_to_non_dict_var = encode_integer_variable(without_wildcards).is_some()
                || encode_float_variable(without_wildcards).is_some();
            if converts_to_non_dict_var {
                token.types = vec![
                    TokenType::IntVar,
                    TokenType::FloatVar,
                    TokenType::DictionaryVar,
                ];
            } else {
                if token.wildcards.is_any() {
                    if could_be_integer_var(&token.value) {
                        token.types.push(TokenType::IntVar);
                    }
                    if could_be_float_var(&token.value) {
                        token.types.push(TokenType::FloatVar);
                    }
                }
                token.types.push(TokenType::DictionaryVar);
                token.cannot_convert_to_non_dict_var = true;
            }
        } else {
            token.cannot_convert_to_non_dict_var = true;
            if token.wildcards.is_any() {
                token.types.push(TokenType::Logtype);
                if could_be_integer_var(value) {
                    token.types.push(TokenType::IntVar);
                    token.cannot_convert_to_non_dict_var = false;
                }
                if could_be_float_var(value) {
                    token.types.push(TokenType::FloatVar);
                    token.cannot_convert_to_non_dict_var = false;
                }
                token.types.push(TokenType::DictionaryVar);
            } else {
                token.types.push(TokenType::Logtype);
            }
        }
        token
    }

    /// # Returns
    ///
    /// The number of interpretations the subquery enumeration tries for this token: tokens with
    /// an inner `*` have one, since they match any logtype span.
    const fn num_interpretations(&self) -> usize {
        if self.wildcards.is_in_middle {
            1
        } else {
            self.types.len()
        }
    }

    /// Moves to the next interpretation, wrapping around to the first after the last (C++
    /// `change_to_next_possible_type`).
    ///
    /// # Returns
    ///
    /// Whether the token moved without wrapping.
    const fn change_to_next_interpretation(&mut self) -> bool {
        if self.current_type + 1 < self.num_interpretations() {
            self.current_type += 1;
            true
        } else {
            self.current_type = 0;
            false
        }
    }

    /// # Returns
    ///
    /// The type the token is currently tried as.
    fn current_type(&self) -> TokenType {
        *self
            .types
            .get(self.current_type)
            .expect("every token has a type, and `current_type` stays below their count")
    }

    /// # Returns
    ///
    /// Whether the token is currently tried as a variable.
    fn is_var(&self) -> bool {
        matches!(
            self.current_type(),
            TokenType::IntVar | TokenType::FloatVar | TokenType::DictionaryVar
        )
    }
}

/// Where a token has greedy (`*`) wildcards; like C++, these ignore escapes.
#[derive(Clone, Copy, Debug, Default)]
struct GreedyWildcards {
    is_prefix: bool,
    is_suffix: bool,
    is_in_middle: bool,
}

impl GreedyWildcards {
    /// # Returns
    ///
    /// Whether the token has any greedy wildcard.
    const fn is_any(self) -> bool {
        self.is_prefix || self.is_suffix || self.is_in_middle
    }
}

/// Generates the subquery of the current token interpretations.
struct SubqueryGenerator<'generator> {
    search_string: &'generator [u8],
    is_case_sensitive: bool,
    logtypes: &'generator LogTypeDictionary,
    variables: &'generator VariableDictionary,
}

impl SubqueryGenerator<'_> {
    /// Builds the logtype glob and variable tests of the current interpretation and matches the
    /// glob against the logtype dictionary (C++ `generate_logtypes_and_vars_for_subquery`).
    ///
    /// # Returns
    ///
    /// The subquery, or why there is none.
    fn generate(&self, tokens: &[QueryToken]) -> Matchability {
        let mut logtype = Vec::new();
        let mut variables = Vec::new();
        let mut is_glob_check_required = false;
        let mut last_token_end = 0;
        for token in tokens {
            append_constant_to_logtype(
                &self.search_string[last_token_end..token.begin],
                &mut logtype,
            );
            last_token_end = token.end;

            if TokenType::Wildcard == token.current_type() {
                logtype.push(wildcard::ANY_RUN);
            } else if token.wildcards.is_in_middle {
                is_glob_check_required = true;
                logtype.push(wildcard::ANY_RUN);
            } else if !token.is_var() {
                append_constant_to_logtype(&token.value, &mut logtype);
            } else {
                is_glob_check_required = true;
                if !self.add_var_token(token, &mut logtype, &mut variables) {
                    return Matchability::WontMatch;
                }
            }
        }
        if last_token_end < self.search_string.len() {
            append_constant_to_logtype(&self.search_string[last_token_end..], &mut logtype);
        }

        if b"*" == logtype.as_slice() {
            return Matchability::SupersedesAll;
        }
        let glob = wildcard::parse(&logtype);
        let mut matching_logtypes = Vec::new();
        self.logtypes.for_each_entry(|id, entry| {
            if glob.matches(entry.text, self.is_case_sensitive) {
                matching_logtypes.push(id.get());
            }
            true
        });
        IdSet::from_ids(matching_logtypes).map_or(Matchability::WontMatch, |logtypes| {
            Matchability::MayMatch(EncodedSubquery {
                logtypes,
                variables: variables.into_boxed_slice(),
                is_glob_check_required,
            })
        })
    }

    /// Adds the placeholder and variable test of a token tried as a variable (C++
    /// `process_var_token`).
    ///
    /// # Returns
    ///
    /// Whether the token might match a value (`false` when its dictionary lookup is empty).
    fn add_var_token(
        &self,
        token: &QueryToken,
        logtype: &mut Vec<u8>,
        variables: &mut Vec<EncodedVariableTest>,
    ) -> bool {
        if !token.wildcards.is_any() {
            if let Some(encoded) = encode_integer_variable(&token.value) {
                logtype.push(VariablePlaceholder::Integer.as_byte());
                variables.push(EncodedVariableTest::Exact(encoded));
            } else if let Some(encoded) = encode_float_variable(&token.value) {
                logtype.push(VariablePlaceholder::Float.as_byte());
                variables.push(EncodedVariableTest::Exact(encoded));
            } else {
                let unescaped = wildcard::unescape(&token.value);
                let matches = self
                    .variables
                    .find_exact(&[unescaped.as_slice()], self.is_case_sensitive);
                let Some(ids) = matches
                    .get(unescaped.as_slice())
                    .and_then(|ids| IdSet::from_ids(ids.iter().map(|id| id.get()).collect()))
                else {
                    return false;
                };
                logtype.push(VariablePlaceholder::Dictionary.as_byte());
                variables.push(EncodedVariableTest::DictionaryIds(ids));
            }
            return true;
        }

        if token.wildcards.is_prefix {
            logtype.push(wildcard::ANY_RUN);
        }
        match token.current_type() {
            TokenType::FloatVar => logtype.push(VariablePlaceholder::Float.as_byte()),
            TokenType::IntVar => logtype.push(VariablePlaceholder::Integer.as_byte()),
            _ => {
                logtype.push(VariablePlaceholder::Dictionary.as_byte());
                if token.cannot_convert_to_non_dict_var {
                    let glob = wildcard::parse(&token.value);
                    let mut ids = Vec::new();
                    self.variables.for_each_entry(|id, entry| {
                        if glob.matches(entry, self.is_case_sensitive) {
                            ids.push(id.get());
                        }
                        true
                    });
                    let Some(ids) = IdSet::from_ids(ids) else {
                        return false;
                    };
                    variables.push(EncodedVariableTest::DictionaryIds(ids));
                }
            }
        }
        if token.wildcards.is_suffix {
            logtype.push(wildcard::ANY_RUN);
        }
        true
    }
}

/// The scanning state of C++ `GrepCore::get_bounds_of_next_potential_var`.
#[derive(Default)]
struct TokenBounds {
    begin: usize,
    end: usize,
    is_var: bool,
}

impl TokenBounds {
    /// Finds the next potential variable: a definite variable or a token with wildcards.
    ///
    /// # Returns
    ///
    /// Whether another token was found; its bounds are `self.begin..self.end`.
    fn advance(&mut self, value: &[u8]) -> bool {
        let length = value.len();
        if self.end >= length {
            return false;
        }
        self.is_var = false;
        let mut contains_wildcard = false;
        while !self.is_var && !contains_wildcard && self.begin < length {
            self.begin = self.end;

            let mut is_escaped = false;
            while self.begin < length {
                let byte = value[self.begin];
                if is_escaped {
                    is_escaped = false;
                    if !is_delimiter(byte) {
                        // Keeps the escape character in the token.
                        self.begin -= 1;
                        break;
                    }
                } else if wildcard::ESCAPE == byte {
                    is_escaped = true;
                } else {
                    if wildcard::is_wildcard(byte) {
                        contains_wildcard = true;
                        break;
                    }
                    if !is_delimiter(byte) {
                        break;
                    }
                }
                self.begin += 1;
            }

            let mut contains_decimal_digit = false;
            let mut contains_alphabet = false;
            is_escaped = false;
            self.end = self.begin;
            while self.end < length {
                let byte = value[self.end];
                if is_escaped {
                    is_escaped = false;
                    if is_delimiter(byte) {
                        // Keeps the escape character out of the token.
                        self.end -= 1;
                        break;
                    }
                } else if wildcard::ESCAPE == byte {
                    is_escaped = true;
                } else if wildcard::is_wildcard(byte) {
                    contains_wildcard = true;
                } else if is_delimiter(byte) {
                    break;
                }
                if byte.is_ascii_digit() {
                    contains_decimal_digit = true;
                } else if byte.is_ascii_alphabetic() {
                    contains_alphabet = true;
                }
                self.end += 1;
            }

            let variable = &value[self.begin..self.end];
            if contains_decimal_digit || could_be_multi_digit_hex_value(variable) {
                self.is_var = true;
            } else if self.begin > 0 && b'=' == value[self.begin - 1] && contains_alphabet {
                let mut is_escaped = false;
                let mut found_wildcard_before_alphabet = false;
                for &byte in variable {
                    if is_escaped {
                        is_escaped = false;
                        if byte.is_ascii_alphabetic() {
                            break;
                        }
                    } else if wildcard::ESCAPE == byte {
                        is_escaped = true;
                    } else if wildcard::is_wildcard(byte) {
                        found_wildcard_before_alphabet = true;
                        break;
                    }
                }
                if !found_wildcard_before_alphabet {
                    self.is_var = true;
                }
            }
        }
        length != self.begin
    }
}

/// Appends a constant of the search string (a C++ wildcard string) to a logtype glob: each
/// literal byte as the logtype stores it (escape and placeholder bytes behind an escape byte),
/// written as a glob literal, and each wildcard as itself.
fn append_constant_to_logtype(constant: &[u8], logtype: &mut Vec<u8>) {
    let push_literal = |byte: u8, logtype: &mut Vec<u8>| {
        if wildcard::is_wildcard(byte) || wildcard::ESCAPE == byte {
            logtype.push(wildcard::ESCAPE);
        }
        logtype.push(byte);
    };
    let escape = VariablePlaceholder::Escape.as_byte();
    let mut bytes = constant.iter().copied();
    while let Some(byte) = bytes.next() {
        let literal = match byte {
            wildcard::ESCAPE => {
                // Tokens and constants never split an escape from the byte it escapes.
                bytes
                    .next()
                    .expect("a constant never ends with a dangling escape")
            }
            wildcard::ANY_RUN | wildcard::ANY_BYTE => {
                logtype.push(byte);
                continue;
            }
            _ => byte,
        };
        if escape == literal || is_variable_placeholder(literal) {
            push_literal(escape, logtype);
        }
        push_literal(literal, logtype);
    }
}

/// # Returns
///
/// Whether `byte` is an integer, dictionary or float placeholder (`ir::is_variable_placeholder`).
const fn is_variable_placeholder(byte: u8) -> bool {
    matches!(
        VariablePlaceholder::from_byte(byte),
        Some(
            VariablePlaceholder::Integer
                | VariablePlaceholder::Dictionary
                | VariablePlaceholder::Float
        )
    )
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
/// Whether `token` has at least two bytes, all of them hex digits
/// (`ir::could_be_multi_digit_hex_value`).
fn could_be_multi_digit_hex_value(token: &[u8]) -> bool {
    token.len() >= 2 && token.iter().all(u8::is_ascii_hexdigit)
}

/// # Returns
///
/// Whether a wildcard string could stand for an integer variable
/// (`wildcard_string_could_be_representable_integer_var`).
fn could_be_integer_var(value: &[u8]) -> bool {
    !value.is_empty()
        && value
            .iter()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'-' | b'?' | b'*'))
}

/// # Returns
///
/// Whether a wildcard string could stand for a float variable
/// (`wildcard_string_could_be_representable_float_var`).
fn could_be_float_var(value: &[u8]) -> bool {
    !value.is_empty()
        && value
            .iter()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'-' | b'.' | b'?' | b'*'))
}

#[cfg(test)]
mod tests {
    use super::QueryToken;
    use super::TokenBounds;
    use super::TokenType;
    use super::append_constant_to_logtype;

    /// # Returns
    ///
    /// The `(token, is_var)` pairs `get_bounds_of_next_potential_var` finds in `text`.
    fn tokens(text: &[u8]) -> Vec<(String, bool)> {
        let mut bounds = TokenBounds::default();
        let mut found = Vec::new();
        while bounds.advance(text) {
            found.push((
                String::from_utf8_lossy(&text[bounds.begin..bounds.end]).into_owned(),
                bounds.is_var,
            ));
        }
        found
    }

    /// # Returns
    ///
    /// `pairs` with owned token texts.
    fn owned(pairs: &[(&str, bool)]) -> Vec<(String, bool)> {
        pairs
            .iter()
            .map(|(token, is_var)| ((*token).to_owned(), *is_var))
            .collect()
    }

    #[test]
    fn potential_variables_follow_the_cpp_rules() {
        for (text, expected) in [
            (&b"request 7 failed"[..], owned(&[("7", true)])),
            (b"hello world", owned(&[])),
            (b"abc", owned(&[("abc", true)])),
            (b"user=alice", owned(&[("alice", true)])),
            (b"user=*lice", owned(&[("*lice", false)])),
            (b"*error*", owned(&[("*error*", false)])),
            (b"took 1*3 ms", owned(&[("1*3", true)])),
            (b"a * b", owned(&[("*", false)])),
            (b"v1.2.3 x", owned(&[("v1.2.3", true)])),
            (br"a\*b 5", owned(&[("5", true)])),
            (
                "caf\u{e9}x 12".as_bytes(),
                owned(&[("caf", true), ("12", true)]),
            ),
        ] {
            assert_eq!(
                tokens(text),
                expected,
                "{:?}",
                String::from_utf8_lossy(text)
            );
        }
    }

    #[test]
    fn tokens_are_typed_like_cpp_query_tokens() {
        let token =
            |text: &str, is_var: bool| QueryToken::new(text.as_bytes(), 0, text.len(), is_var);

        let integer = token("123", true);
        assert_eq!(
            integer.types,
            [
                TokenType::IntVar,
                TokenType::FloatVar,
                TokenType::DictionaryVar
            ]
        );
        assert!(!integer.cannot_convert_to_non_dict_var);

        let dictionary = token("v1.2.3", true);
        assert_eq!(dictionary.types, [TokenType::DictionaryVar]);
        assert!(dictionary.cannot_convert_to_non_dict_var);

        let wildcard_number = token("*12*", true);
        assert_eq!(wildcard_number.num_interpretations(), 3);
        assert!(wildcard_number.wildcards.is_prefix && wildcard_number.wildcards.is_suffix);

        let text = token("*error*", false);
        assert_eq!(text.types, [TokenType::Logtype, TokenType::DictionaryVar]);
        assert!(text.cannot_convert_to_non_dict_var);

        let digits = token("*12", false);
        assert_eq!(
            digits.types,
            [
                TokenType::Logtype,
                TokenType::IntVar,
                TokenType::FloatVar,
                TokenType::DictionaryVar
            ]
        );
        assert!(!digits.cannot_convert_to_non_dict_var);

        assert_eq!(token("*", false).types, [TokenType::Wildcard]);
        assert_eq!(token("hello", false).types, [TokenType::Logtype]);
    }

    #[test]
    fn inner_wildcard_tokens_have_one_interpretation() {
        let mut token = QueryToken::new(b"1*3", 0, 3, true);
        assert!(token.wildcards.is_in_middle);
        assert_eq!(token.num_interpretations(), 1);
        assert!(!token.change_to_next_interpretation());

        let text = QueryToken::new(b"ab*cd", 0, 5, false);
        assert_eq!(text.num_interpretations(), 1);
        assert!(!text.is_var());
    }

    #[test]
    fn partial_numbers_with_greedy_wildcards_try_numeric_placeholders() {
        let token = QueryToken::new(b"*0.*", 0, 4, true);
        assert_eq!(token.types, [TokenType::FloatVar, TokenType::DictionaryVar]);
        assert!(token.cannot_convert_to_non_dict_var);

        let dash = QueryToken::new(b"*5-", 0, 3, true);
        assert_eq!(
            dash.types,
            [
                TokenType::IntVar,
                TokenType::FloatVar,
                TokenType::DictionaryVar
            ]
        );

        let exact = QueryToken::new(b"0.", 0, 2, true);
        assert_eq!(exact.types, [TokenType::DictionaryVar]);
    }

    #[test]
    fn constants_become_logtype_globs() {
        for (constant, expected) in [
            (&b"plain text"[..], &b"plain text"[..]),
            (b"a\x11b", b"a\\\\\x11b"),
            (b"a\x11*", b"a\\\\\x11*"),
            (br"a\*", br"a\*"),
            (br"a\?", br"a\?"),
            (br"a\\b", br"a\\\\b"),
            (br"a\\", br"a\\\\"),
            (br"a\\*", br"a\\\\*"),
        ] {
            let mut logtype = Vec::new();
            append_constant_to_logtype(constant, &mut logtype);
            assert_eq!(logtype, expected, "{:?}", String::from_utf8_lossy(constant));
        }
    }
}
