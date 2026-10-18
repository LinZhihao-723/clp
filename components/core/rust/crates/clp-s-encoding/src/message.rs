//! Message reconstruction from a logtype and its encoded variables
//! (`clp::EncodedVariableInterpreter`, `clp_s::LogTypeDictionaryEntry`), and the inverse encoding.

use crate::EncodedVariable;
use crate::EncodingError;
use crate::VariablePlaceholder;
use crate::VariableTokenizer;
use crate::decode_dictionary_variable;
use crate::decode_float_variable;
use crate::decode_integer_variable;
use crate::encode_dictionary_variable;
use crate::encode_float_variable;
use crate::encode_integer_variable;

/// Iterates over the placeholders of a logtype, with their byte offsets, exactly as the C++ reader
/// indexes them (`LogTypeDictionaryEntry::decode_log_type`).
///
/// The byte after an escape is never a placeholder, so an escaped placeholder byte (or an escaped
/// escape) is part of the constant text.
#[derive(Clone, Debug)]
pub struct LogtypePlaceholders<'logtype> {
    logtype: &'logtype [u8],
    position: usize,
}

impl<'logtype> LogtypePlaceholders<'logtype> {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// An iterator over the placeholders of `logtype`.
    #[must_use]
    pub const fn new(logtype: &'logtype [u8]) -> Self {
        Self {
            logtype,
            position: 0,
        }
    }
}

impl Iterator for LogtypePlaceholders<'_> {
    type Item = (usize, VariablePlaceholder);

    fn next(&mut self) -> Option<Self::Item> {
        while self.position < self.logtype.len() {
            let offset = self.position;
            self.position += 1;
            if let Some(placeholder) = VariablePlaceholder::from_byte(self.logtype[offset]) {
                if VariablePlaceholder::Escape == placeholder {
                    self.position += 1;
                }
                return Some((offset, placeholder));
            }
        }
        None
    }
}

/// Appends the message encoded by `logtype` and `variables` to `output`, as
/// `EncodedVariableInterpreter::decode_variables_into_message` reconstructs it.
///
/// Constants between placeholders are copied verbatim; an escape placeholder is dropped (so the
/// byte it escapes is kept, and a trailing escape vanishes); each variable placeholder consumes the
/// next variable. On failure, `output` is left unchanged.
///
/// # Type Parameters
///
/// * `PlaceholdersType` - The placeholders of `logtype` with their byte offsets, in increasing
///   offset order, e.g. [`LogtypePlaceholders`] or offsets indexed when a dictionary was loaded.
/// * `LookupType` - Resolves a variable-dictionary id to the entry's bytes, or `None` if the id is
///   unknown.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`EncodingError::InvalidPlaceholderOffset`] if a placeholder offset is out of order, out of
///   range, or does not point at a placeholder byte of the given kind.
/// * [`EncodingError::VariableCountMismatch`] if the number of variable placeholders differs from
///   `variables.len()`.
/// * [`EncodingError::UnknownDictionaryVariable`] if `lookup` does not know a dictionary variable's
///   id.
/// * Forwards [`decode_float_variable`]'s return values on failure.
pub fn decode_message<
    'dictionary,
    PlaceholdersType: IntoIterator<Item = (usize, VariablePlaceholder)>,
    LookupType: FnMut(u64) -> Option<&'dictionary [u8]>,
>(
    logtype: &[u8],
    placeholders: PlaceholdersType,
    variables: &[EncodedVariable],
    lookup: LookupType,
    output: &mut Vec<u8>,
) -> Result<(), EncodingError> {
    let original_length = output.len();
    let result = append_decoded_message(logtype, placeholders, variables, lookup, output);
    if result.is_err() {
        output.truncate(original_length);
    }
    result
}

/// Appends the logtype and variables of `message` to `logtype` and `variables`.
///
/// This mirrors `EncodedVariableInterpreter::encode_and_add_to_dictionary`: every token of
/// [`VariableTokenizer`] becomes a placeholder and a variable, and placeholder and escape bytes in
/// the constants are escaped.
///
/// # Type Parameters
///
/// * `AddDictionaryVariableType` - Adds a dictionary variable's text to the variable dictionary and
///   returns its id.
pub fn encode_message<AddDictionaryVariableType: FnMut(&[u8]) -> u64>(
    message: &[u8],
    logtype: &mut Vec<u8>,
    variables: &mut Vec<EncodedVariable>,
    mut add_dictionary_variable: AddDictionaryVariableType,
) {
    let mut constant_begin = 0;
    for token in VariableTokenizer::new(message) {
        append_escaped_constant(&message[constant_begin..token.range.start], logtype);
        let text = &message[token.range.clone()];
        let (placeholder, variable) = encode_integer_variable(text)
            .map(|variable| (VariablePlaceholder::Integer, variable))
            .or_else(|| {
                encode_float_variable(text).map(|variable| (VariablePlaceholder::Float, variable))
            })
            .unwrap_or_else(|| {
                (
                    VariablePlaceholder::Dictionary,
                    encode_dictionary_variable(add_dictionary_variable(text)),
                )
            });
        logtype.push(placeholder.as_byte());
        variables.push(variable);
        constant_begin = token.range.end;
    }
    append_escaped_constant(&message[constant_begin..], logtype);
}

/// Appends the decoded message to `output`, leaving partial output behind on failure.
///
/// # Type Parameters
///
/// * `PlaceholdersType` - See [`decode_message`].
/// * `LookupType` - See [`decode_message`].
///
/// # Errors
///
/// Returns an error if:
///
/// * [`EncodingError::InvalidPlaceholderOffset`] if a placeholder offset is invalid.
/// * [`EncodingError::VariableCountMismatch`] if the variable count differs from the number of
///   variable placeholders.
/// * [`EncodingError::UnknownDictionaryVariable`] if `lookup` does not know an id.
/// * Forwards [`decode_float_variable`]'s return values on failure.
fn append_decoded_message<
    'dictionary,
    PlaceholdersType: IntoIterator<Item = (usize, VariablePlaceholder)>,
    LookupType: FnMut(u64) -> Option<&'dictionary [u8]>,
>(
    logtype: &[u8],
    placeholders: PlaceholdersType,
    variables: &[EncodedVariable],
    mut lookup: LookupType,
    output: &mut Vec<u8>,
) -> Result<(), EncodingError> {
    let mut constant_begin = 0;
    let mut remaining_variables = variables.iter();
    let mut placeholders = placeholders.into_iter();
    while let Some((offset, placeholder)) = placeholders.next() {
        if offset < constant_begin || logtype.get(offset).copied() != Some(placeholder.as_byte()) {
            return Err(EncodingError::InvalidPlaceholderOffset { offset });
        }
        output.extend_from_slice(&logtype[constant_begin..offset]);
        constant_begin = offset + 1;
        match placeholder {
            VariablePlaceholder::Escape => {}
            VariablePlaceholder::Integer => {
                let variable =
                    next_variable(&mut remaining_variables, &mut placeholders, variables.len())?;
                decode_integer_variable(variable, output);
            }
            VariablePlaceholder::Float => {
                let variable =
                    next_variable(&mut remaining_variables, &mut placeholders, variables.len())?;
                decode_float_variable(variable, output)?;
            }
            VariablePlaceholder::Dictionary => {
                let variable =
                    next_variable(&mut remaining_variables, &mut placeholders, variables.len())?;
                let id = decode_dictionary_variable(variable);
                let entry = lookup(id).ok_or(EncodingError::UnknownDictionaryVariable { id })?;
                output.extend_from_slice(entry);
            }
        }
    }

    let num_unused = remaining_variables.len();
    if 0 != num_unused {
        return Err(EncodingError::VariableCountMismatch {
            num_placeholders: variables.len() - num_unused,
            num_variables: variables.len(),
        });
    }
    if constant_begin < logtype.len() {
        output.extend_from_slice(&logtype[constant_begin..]);
    }
    Ok(())
}

/// Takes the variable of the variable placeholder just read.
///
/// # Type Parameters
///
/// * `PlaceholderIteratorType` - The placeholders after the one just read.
///
/// # Returns
///
/// The next variable on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`EncodingError::VariableCountMismatch`] if all `num_variables` variables are already taken;
///   the reported placeholder count includes the variable placeholders left in
///   `remaining_placeholders`.
fn next_variable<PlaceholderIteratorType: Iterator<Item = (usize, VariablePlaceholder)>>(
    remaining_variables: &mut std::slice::Iter<'_, EncodedVariable>,
    remaining_placeholders: &mut PlaceholderIteratorType,
    num_variables: usize,
) -> Result<EncodedVariable, EncodingError> {
    if let Some(&variable) = remaining_variables.next() {
        return Ok(variable);
    }
    let num_missing = 1 + remaining_placeholders
        .filter(|(_, placeholder)| VariablePlaceholder::Escape != *placeholder)
        .count();
    Err(EncodingError::VariableCountMismatch {
        num_placeholders: num_variables + num_missing,
        num_variables,
    })
}

/// Appends `constant` to `logtype`, preceding every placeholder or escape byte with an escape
/// (`clp::ir::append_constant_to_logtype`).
fn append_escaped_constant(constant: &[u8], logtype: &mut Vec<u8>) {
    for &byte in constant {
        if VariablePlaceholder::from_byte(byte).is_some() {
            logtype.push(VariablePlaceholder::Escape.as_byte());
        }
        logtype.push(byte);
    }
}

#[cfg(test)]
mod tests {
    use super::LogtypePlaceholders;
    use super::decode_message;
    use super::encode_message;
    use crate::EncodedVariable;
    use crate::EncodingError;
    use crate::VariablePlaceholder;
    use crate::encode_dictionary_variable;
    use crate::encode_float_variable;

    /// A variable dictionary for tests: entry `i` has id `i`.
    #[derive(Default)]
    struct Dictionary {
        entries: Vec<Vec<u8>>,
    }

    impl Dictionary {
        /// # Returns
        ///
        /// The id of `text`, adding it if it is new.
        fn add(&mut self, text: &[u8]) -> u64 {
            let index = self
                .entries
                .iter()
                .position(|entry| entry == text)
                .unwrap_or_else(|| {
                    self.entries.push(text.to_vec());
                    self.entries.len() - 1
                });
            u64::try_from(index).expect("test dictionaries are small")
        }

        /// # Returns
        ///
        /// The entry with `id`, or `None` if there is none.
        fn get(&self, id: u64) -> Option<&[u8]> {
            usize::try_from(id)
                .ok()
                .and_then(|index| self.entries.get(index))
                .map(Vec::as_slice)
        }
    }

    /// # Returns
    ///
    /// The message decoded from `logtype` and `variables`, with placeholders found by
    /// [`LogtypePlaceholders`].
    ///
    /// # Errors
    ///
    /// Forwards [`decode_message`]'s return values on failure.
    fn decode(
        logtype: &[u8],
        variables: &[EncodedVariable],
        dictionary: &Dictionary,
    ) -> Result<Vec<u8>, EncodingError> {
        let mut output = Vec::new();
        decode_message(
            logtype,
            LogtypePlaceholders::new(logtype),
            variables,
            |id| dictionary.get(id),
            &mut output,
        )?;
        Ok(output)
    }

    #[test]
    fn placeholders_skip_escaped_bytes() {
        let logtype = b"a\x11b\\\x12c\\\\\x13d\\";
        let placeholders: Vec<_> = LogtypePlaceholders::new(logtype).collect();
        assert_eq!(
            placeholders,
            vec![
                (1, VariablePlaceholder::Integer),
                (3, VariablePlaceholder::Escape),
                (6, VariablePlaceholder::Escape),
                (8, VariablePlaceholder::Float),
                (10, VariablePlaceholder::Escape),
            ]
        );
    }

    #[test]
    fn messages_decode_like_cpp() -> anyhow::Result<()> {
        let mut dictionary = Dictionary::default();
        let user_id = dictionary.add(b"alice");
        let float = encode_float_variable(b"-0.250").expect("the float should be encodable");

        let logtype = b"user \x12 took \x11 ms at \x13 (\\\x11 literal, \\\\ backslash)\\";
        let variables = [encode_dictionary_variable(user_id), 42, float];
        assert_eq!(
            decode(logtype, &variables, &dictionary)?,
            b"user alice took 42 ms at -0.250 (\x11 literal, \\ backslash)"
        );
        assert_eq!(decode(b"", &[], &dictionary)?, b"");
        assert_eq!(decode(b"\x11", &[-7], &dictionary)?, b"-7");
        Ok(())
    }

    #[test]
    fn decoding_appends_to_existing_output() -> anyhow::Result<()> {
        let logtype = b"n=\x11";
        let mut output = b"{\"m\":".to_vec();
        decode_message(
            logtype,
            LogtypePlaceholders::new(logtype),
            &[5],
            |_| None,
            &mut output,
        )?;
        assert_eq!(output, b"{\"m\":n=5");
        Ok(())
    }

    #[test]
    fn variable_count_mismatches_are_errors() {
        let dictionary = Dictionary::default();
        let result = decode(b"\x11 \x11 \\\x11 \x13", &[1], &dictionary);
        assert!(
            matches!(
                result,
                Err(EncodingError::VariableCountMismatch {
                    num_placeholders: 3,
                    num_variables: 1,
                })
            ),
            "{result:?}"
        );

        let result = decode(b"\x11", &[1, 2, 3], &dictionary);
        assert!(
            matches!(
                result,
                Err(EncodingError::VariableCountMismatch {
                    num_placeholders: 1,
                    num_variables: 3,
                })
            ),
            "{result:?}"
        );
    }

    #[test]
    fn unknown_dictionary_ids_and_bad_floats_are_errors() {
        let dictionary = Dictionary::default();
        let result = decode(b"\x12", &[encode_dictionary_variable(9)], &dictionary);
        assert!(
            matches!(
                result,
                Err(EncodingError::UnknownDictionaryVariable { id: 9 })
            ),
            "{result:?}"
        );

        let invalid_float = ((5_u64 << 8) | 0x12).cast_signed();
        let result = decode(b"\x13", &[invalid_float], &dictionary);
        assert!(
            matches!(result, Err(EncodingError::InvalidEncodedFloat { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn invalid_placeholder_offsets_are_errors_and_leave_output_unchanged() {
        let logtype = b"ab\x11cd\x11";
        for placeholders in [
            vec![
                (5, VariablePlaceholder::Integer),
                (2, VariablePlaceholder::Integer),
            ],
            vec![(1, VariablePlaceholder::Integer)],
            vec![(2, VariablePlaceholder::Float)],
            vec![(9, VariablePlaceholder::Integer)],
        ] {
            let mut output = b"kept".to_vec();
            let result = decode_message(
                logtype,
                placeholders.clone(),
                &[1, 2],
                |_| None,
                &mut output,
            );
            assert!(
                matches!(result, Err(EncodingError::InvalidPlaceholderOffset { .. })),
                "placeholders {placeholders:?}: {result:?}"
            );
            assert_eq!(output, b"kept");
        }
    }

    #[test]
    fn messages_round_trip_through_encoding() -> anyhow::Result<()> {
        for message in [
            "",
            "plain text without variables",
            "INFO took 123 ms (avg 4.56, max -7) user=alice id=0xdeadbeef",
            "escapes \\ and placeholder bytes \x11\x12\x13 survive",
            "trailing backslash \\",
            "v1.2.3 build_42 +1 -0 007 1. .5 -.5 9223372036854775808",
            "unicode caf\u{e9}1 \u{1f600} 12\u{2028}34",
            "12345678901234567.0 1234567890123456.0 00.010",
        ] {
            let mut dictionary = Dictionary::default();
            let mut logtype = Vec::new();
            let mut variables = Vec::new();
            encode_message(message.as_bytes(), &mut logtype, &mut variables, |text| {
                dictionary.add(text)
            });
            assert_eq!(
                decode(&logtype, &variables, &dictionary)?,
                message.as_bytes(),
                "message {message:?}"
            );
        }
        Ok(())
    }

    // The message of the "Test encoding and decoding" section of
    // `components/core/tests/test-EncodedVariableInterpreter.cpp`.
    #[test]
    fn cpp_interpreter_message_round_trips() -> anyhow::Result<()> {
        let large_integer = format!("{}0", u64::MAX);
        let message = format!(
            "here is a string with a small int 4938 and a very large int {large_integer} and a \
             double -25.5196868642755 and a weird double -00.00 and a str with numbers \
             python2.7.3 and a str with a backslash and numbers \\a1 and an escape \\ and an int \
             placeholder \x11 and a float placeholder \x13 and a dictionary placeholder \x12"
        );
        let mut dictionary = Dictionary::default();
        let mut logtype = Vec::new();
        let mut variables = Vec::new();
        encode_message(message.as_bytes(), &mut logtype, &mut variables, |text| {
            dictionary.add(text)
        });

        let kinds: Vec<_> = LogtypePlaceholders::new(&logtype)
            .map(|(_, placeholder)| placeholder)
            .collect();
        assert_eq!(
            kinds,
            vec![
                VariablePlaceholder::Integer,
                VariablePlaceholder::Dictionary,
                VariablePlaceholder::Float,
                VariablePlaceholder::Float,
                VariablePlaceholder::Dictionary,
                VariablePlaceholder::Dictionary,
                VariablePlaceholder::Escape,
                VariablePlaceholder::Escape,
                VariablePlaceholder::Escape,
                VariablePlaceholder::Escape,
            ]
        );
        assert_eq!(
            dictionary.entries,
            vec![
                large_integer.into_bytes(),
                b"python2.7.3".to_vec(),
                b"\\a1".to_vec()
            ]
        );
        assert_eq!(
            decode(&logtype, &variables, &dictionary)?,
            message.as_bytes()
        );
        Ok(())
    }

    #[test]
    fn encoding_matches_cpp_logtype_layout() {
        let mut dictionary = Dictionary::default();
        let mut logtype = Vec::new();
        let mut variables = Vec::new();
        encode_message(
            b"id=bob took 12 ms, ratio 0.5 \\ \x11",
            &mut logtype,
            &mut variables,
            |text| dictionary.add(text),
        );
        assert_eq!(logtype, b"id=\x12 took \x11 ms, ratio \x13 \\\\ \\\x11");
        assert_eq!(
            variables,
            vec![
                encode_dictionary_variable(0),
                12,
                encode_float_variable(b"0.5").expect("the float should be encodable"),
            ]
        );
        assert_eq!(dictionary.entries, vec![b"bob".to_vec()]);
    }
}
