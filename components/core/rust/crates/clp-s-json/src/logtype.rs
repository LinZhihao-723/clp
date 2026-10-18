//! Compiled logtypes for the fused `ClpString` decode-and-escape writer.

use std::sync::OnceLock;

use clp_s_encoding::VariablePlaceholder;
use clp_s_encoding::decode_dictionary_variable;
use clp_s_encoding::decode_float_variable;
use clp_s_encoding::decode_integer_variable;
use clp_s_format::LogTypeDictionary;
use clp_s_format::LogTypeId;
use clp_s_format::Logtype;
use clp_s_format::VariableDictionary;
use clp_s_format::VariableId;
use clp_s_table::LeI64;
use clp_s_table::TableError;

use crate::escape::escape_into;

/// The compiled logtypes of one archive's logtype dictionary, compiled on first use in lazily
/// allocated chunks of slots.
#[derive(Debug, Default)]
pub struct LogtypeCache {
    chunks: OnceLock<Box<[OnceLock<Chunk>]>>,
}

impl LogtypeCache {
    const CHUNK_SIZE: usize = 256;

    /// Appends the JSON-escaped text of the `ClpString` with logtype `id` and `variables` to
    /// `output`.
    ///
    /// On failure, `output` may hold a partial value.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::Format`] if `id` is out of range.
    /// * Forwards [`CompiledLogtype::write`]'s return values on failure.
    ///
    /// # Panics
    ///
    /// Panics if `logtypes` is not the dictionary the cache was first used with.
    pub fn write(
        &self,
        logtypes: &LogTypeDictionary,
        id: LogTypeId,
        variables: &[LeI64],
        variable_dictionary: &VariableDictionary,
        output: &mut Vec<u8>,
    ) -> Result<(), TableError> {
        let logtype = logtypes.get(id)?;
        let chunks = self.chunks.get_or_init(|| {
            let num_entries = usize::try_from(logtypes.len()).unwrap_or(usize::MAX);
            (0..num_entries.div_ceil(Self::CHUNK_SIZE))
                .map(|_| OnceLock::new())
                .collect()
        });
        let index = usize::try_from(id.get()).unwrap_or(usize::MAX);
        let chunk = chunks
            .get(index / Self::CHUNK_SIZE)
            .expect("the cache is only used with the dictionary it was sized for");
        let slot = &chunk.get_or_init(|| (0..Self::CHUNK_SIZE).map(|_| OnceLock::new()).collect())
            [index % Self::CHUNK_SIZE];
        slot.get_or_init(|| CompiledLogtype::compile(&logtype))
            .write(variables, variable_dictionary, output)
    }
}

/// The slots of [`LogtypeCache::CHUNK_SIZE`] consecutive logtype ids.
type Chunk = Box<[OnceLock<CompiledLogtype>]>;

/// A logtype with its constant text pre-escaped and its variables listed in order.
#[derive(Debug)]
struct CompiledLogtype {
    /// Every constant piece, escaped, back to back.
    escaped_constants: Box<[u8]>,

    variables: Box<[CompiledVariable]>,
}

impl CompiledLogtype {
    /// Factory function.
    ///
    /// Splits `logtype` the way `EncodedVariableInterpreter::decode_variables_into_message`
    /// reads it: an escape placeholder is dropped and the byte it escapes stays constant text.
    ///
    /// # Returns
    ///
    /// The compiled logtype.
    fn compile(logtype: &Logtype<'_>) -> Self {
        let text = logtype.text;
        let mut escaped_constants = Vec::with_capacity(text.len());
        let mut variables = Vec::new();
        let mut constant_begin = 0;
        for placeholder in logtype.placeholders {
            let offset = placeholder.offset as usize;
            escape_into(&text[constant_begin..offset], &mut escaped_constants);
            constant_begin = offset + 1;
            if VariablePlaceholder::Escape != placeholder.kind {
                variables.push(CompiledVariable {
                    constants_end: escaped_constants.len(),
                    kind: placeholder.kind,
                });
            }
        }
        escape_into(&text[constant_begin..], &mut escaped_constants);
        Self {
            escaped_constants: escaped_constants.into_boxed_slice(),
            variables: variables.into_boxed_slice(),
        }
    }

    /// Appends the escaped text of the logtype filled with `variables` to `output`.
    ///
    /// Integer and float variables decode to `-`, digits and `.`, which never need escaping;
    /// dictionary variables are escaped.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::Format`] if a variable-dictionary id is out of range.
    /// * [`TableError::Encoding`] if an encoded float variable is invalid.
    fn write(
        &self,
        variables: &[LeI64],
        variable_dictionary: &VariableDictionary,
        output: &mut Vec<u8>,
    ) -> Result<(), TableError> {
        let mut constants_begin = 0;
        for (variable, encoded) in self.variables.iter().zip(variables) {
            output.extend_from_slice(
                &self.escaped_constants[constants_begin..variable.constants_end],
            );
            constants_begin = variable.constants_end;
            let encoded = encoded.get();
            match variable.kind {
                VariablePlaceholder::Integer => decode_integer_variable(encoded, output),
                VariablePlaceholder::Float => decode_float_variable(encoded, output)?,
                VariablePlaceholder::Dictionary => {
                    let id = VariableId::new(decode_dictionary_variable(encoded));
                    escape_into(variable_dictionary.get(id)?, output);
                }
                VariablePlaceholder::Escape => {}
            }
        }
        output.extend_from_slice(&self.escaped_constants[constants_begin..]);
        Ok(())
    }
}

/// One variable of a [`CompiledLogtype`].
#[derive(Clone, Copy, Debug)]
struct CompiledVariable {
    /// The end of the escaped constants that precede this variable.
    constants_end: usize,

    kind: VariablePlaceholder,
}

#[cfg(test)]
mod tests {
    use clp_s_encoding::encode_float_variable;
    use clp_s_format::FormatError;
    use clp_s_format::ReaderLimits;

    use super::*;

    /// # Returns
    ///
    /// A dictionary section (`u64` count, then one zstd frame of `(u64 length, bytes)` records)
    /// holding `entries`.
    fn dictionary_section(entries: &[&[u8]]) -> Vec<u8> {
        let mut records = Vec::new();
        for entry in entries {
            records.extend_from_slice(&(entry.len() as u64).to_le_bytes());
            records.extend_from_slice(entry);
        }
        let mut section = (entries.len() as u64).to_le_bytes().to_vec();
        section.extend(zstd::encode_all(records.as_slice(), 1).expect("zstd compresses"));
        section
    }

    /// # Returns
    ///
    /// The logtype dictionary holding `entries`.
    fn logtypes(entries: &[&[u8]]) -> LogTypeDictionary {
        LogTypeDictionary::load(&dictionary_section(entries), &ReaderLimits::default())
            .expect("the logtype dictionary loads")
    }

    /// # Returns
    ///
    /// The variable dictionary holding `entries`.
    fn variables(entries: &[&[u8]]) -> VariableDictionary {
        VariableDictionary::load(&dictionary_section(entries), &ReaderLimits::default())
            .expect("the variable dictionary loads")
    }

    /// # Returns
    ///
    /// The escaped text of logtype `id` of `logtypes` with the variables `encoded`, written
    /// through `cache`, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`LogtypeCache::write`]'s return values on failure.
    fn write(
        cache: &LogtypeCache,
        logtypes: &LogTypeDictionary,
        id: u64,
        encoded: &[i64],
        dictionary: &VariableDictionary,
    ) -> Result<Vec<u8>, TableError> {
        let encoded: Vec<LeI64> = encoded.iter().copied().map(LeI64::new).collect();
        let mut output = Vec::new();
        cache.write(
            logtypes,
            LogTypeId::new(id),
            &encoded,
            dictionary,
            &mut output,
        )?;
        Ok(output)
    }

    #[test]
    fn writes_every_variable_kind_escaped() -> anyhow::Result<()> {
        let float = encode_float_variable(b"-1.250").expect("the float is encodable");
        let logtypes = logtypes(&[b"say \"\x12\"\tn=\x11 f=\x13\n", b"no variables \\\\ \x01"]);
        let dictionary = variables(&[b"plain", b"q\"uo\\te\x01\n"]);
        let cache = LogtypeCache::default();
        assert_eq!(
            write(&cache, &logtypes, 0, &[1, -42, float], &dictionary)?,
            b"say \\\"q\\\"uo\\\\te\\u0001\\n\\\"\\tn=-42 f=-1.250\\n"
        );
        assert_eq!(
            write(&cache, &logtypes, 0, &[0, 7, float], &dictionary)?,
            b"say \\\"plain\\\"\\tn=7 f=-1.250\\n"
        );
        assert_eq!(
            write(&cache, &logtypes, 1, &[], &dictionary)?,
            b"no variables \\\\ \\u0001"
        );
        Ok(())
    }

    #[test]
    fn escaped_placeholder_bytes_stay_constant_text() -> anyhow::Result<()> {
        let logtypes = logtypes(&[b"a\\\x11b \x11 c\\\\d"]);
        let dictionary = variables(&[]);
        assert_eq!(
            write(&LogtypeCache::default(), &logtypes, 0, &[5], &dictionary)?,
            b"a\\u0011b 5 c\\\\d"
        );
        Ok(())
    }

    #[test]
    fn invalid_ids_and_variables_fail() {
        let logtypes = logtypes(&[b"\x12", b"\x13"]);
        let dictionary = variables(&[b"only"]);
        let cache = LogtypeCache::default();
        for (id, encoded) in [(2, 0), (0, 1)] {
            let result = write(&cache, &logtypes, id, &[encoded], &dictionary);
            assert!(
                matches!(result, Err(TableError::Format(FormatError::Corrupt { .. }))),
                "logtype {id}: {result:?}"
            );
        }
        let result = write(&cache, &logtypes, 1, &[-1], &dictionary);
        assert!(matches!(result, Err(TableError::Encoding(_))), "{result:?}");
    }

    #[test]
    fn compiled_logtypes_are_reused_across_chunks() -> anyhow::Result<()> {
        let entries: Vec<Vec<u8>> = (0..600)
            .map(|id| format!("entry \"{id}\" \x11").into_bytes())
            .collect();
        let logtypes = logtypes(&entries.iter().map(Vec::as_slice).collect::<Vec<_>>());
        let dictionary = variables(&[]);
        let cache = LogtypeCache::default();
        for _ in 0..2 {
            for id in [0_u64, 255, 256, 511, 599] {
                assert_eq!(
                    write(&cache, &logtypes, id, &[9], &dictionary)?,
                    format!("entry \\\"{id}\\\" 9").into_bytes(),
                    "logtype {id}"
                );
            }
        }
        Ok(())
    }
}
