//! Typed, alignment-1 views of one column restricted to a window.

use clp_s_encoding::VariablePlaceholder;
use clp_s_encoding::decode_dictionary_variable;
use clp_s_encoding::decode_float_variable;
use clp_s_encoding::decode_integer_variable;
use clp_s_format::LogTypeDictionary;
use clp_s_format::LogTypeId;
use clp_s_format::Logtype;
use clp_s_format::TableOrdinal;
use clp_s_format::TimestampPatternId;
use clp_s_format::VariableDictionary;
use clp_s_format::VariableId;

use crate::EncodedTextRef;
use crate::FloatRef;
use crate::StringRef;
use crate::TableError;
use crate::ValueRef;
use crate::table::ColumnData;

/// Little-endian, alignment-1 `i64` (ERT columns are packed back to back, so unaligned).
pub type LeI64 = zerocopy::byteorder::little_endian::I64;

/// Little-endian, alignment-1 `u64`.
pub type LeU64 = zerocopy::byteorder::little_endian::U64;

/// Little-endian, alignment-1 `f64`.
pub type LeF64 = zerocopy::byteorder::little_endian::F64;

/// Little-endian, alignment-1 `u16`.
pub type LeU16 = zerocopy::byteorder::little_endian::U16;

/// One column restricted to a window; element `i` is window row `i`. Delta-encoded columns are
/// already prefix-summed.
#[non_exhaustive]
#[derive(Clone, Copy, Debug)]
pub enum ColumnBatch<'batch> {
    Int64(&'batch [LeI64]),
    DeltaInt64(&'batch [i64]),
    Float64(&'batch [LeF64]),
    FormattedFloat {
        values: &'batch [LeF64],
        formats: &'batch [LeU16],
    },
    DictionaryFloat {
        ids: &'batch [LeU64],
        dictionary: &'batch VariableDictionary,
    },
    Boolean(&'batch [u8]),
    ClpString(EncodedTextBatch<'batch>),
    VariableString {
        ids: &'batch [LeU64],
        dictionary: &'batch VariableDictionary,
    },
    UnstructuredArray(EncodedTextBatch<'batch>),
    Timestamp {
        epoch_ns: &'batch [i64],
        pattern_ids: &'batch [LeU64],
    },
}

impl<'batch> ColumnBatch<'batch> {
    /// # Returns
    ///
    /// The number of rows in the window.
    #[must_use]
    pub fn num_rows(&self) -> u32 {
        let num_rows = match self {
            Self::Int64(values) => values.len(),
            Self::DeltaInt64(values) => values.len(),
            Self::Float64(values) | Self::FormattedFloat { values, .. } => values.len(),
            Self::DictionaryFloat { ids, .. } | Self::VariableString { ids, .. } => ids.len(),
            Self::Boolean(values) => values.len(),
            Self::ClpString(batch) | Self::UnstructuredArray(batch) => {
                return batch.num_rows();
            }
            Self::Timestamp { epoch_ns, .. } => epoch_ns.len(),
        };
        u32::try_from(num_rows).unwrap_or(u32::MAX)
    }

    /// Returns the value of window row `row`, resolving dictionary entries (encoded text is left
    /// encoded).
    ///
    /// # Returns
    ///
    /// The value on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::Format`] if a variable-dictionary id is out of range.
    ///
    /// # Panics
    ///
    /// Panics if `row` is not below [`Self::num_rows`].
    pub fn value(&self, row: u32) -> Result<ValueRef<'batch>, TableError> {
        let index = row as usize;
        Ok(match *self {
            Self::Int64(values) => ValueRef::Integer(values[index].get()),
            Self::DeltaInt64(values) => ValueRef::Integer(values[index]),
            Self::Float64(values) => ValueRef::Float(FloatRef::Plain(values[index].get())),
            Self::FormattedFloat { values, formats } => ValueRef::Float(FloatRef::Formatted {
                value: values[index].get(),
                format: formats[index].get(),
            }),
            Self::DictionaryFloat { ids, dictionary } => {
                let id = VariableId::new(ids[index].get());
                ValueRef::Float(FloatRef::Dictionary {
                    id,
                    text: dictionary.get(id)?,
                })
            }
            Self::Boolean(values) => ValueRef::Boolean(0 != values[index]),
            Self::ClpString(batch) => {
                ValueRef::String(StringRef::Encoded(EncodedTextRef::new(batch, row)))
            }
            Self::VariableString { ids, dictionary } => {
                let id = VariableId::new(ids[index].get());
                ValueRef::String(StringRef::Variable {
                    id,
                    text: dictionary.get(id)?,
                })
            }
            Self::UnstructuredArray(batch) => {
                ValueRef::UnstructuredArray(EncodedTextRef::new(batch, row))
            }
            Self::Timestamp {
                epoch_ns,
                pattern_ids,
            } => ValueRef::Timestamp {
                epoch_ns: epoch_ns[index],
                pattern_id: TimestampPatternId::new(pattern_ids[index].get()),
            },
        })
    }
}

/// The window slice of a `ClpString` or unstructured-array column: one logtype word per row plus
/// the variables each row references.
#[derive(Clone, Copy, Debug)]
pub struct EncodedTextBatch<'batch> {
    logtype_words: &'batch [LeU64],

    /// The whole variable region of the column; logtype words index into it.
    variables: &'batch [LeI64],

    logtypes: &'batch LogTypeDictionary,
    variable_dictionary: &'batch VariableDictionary,
    table: TableOrdinal,

    /// The table row of window row 0 (for error reports).
    first_row: u32,
}

impl<'batch> EncodedTextBatch<'batch> {
    /// # Returns
    ///
    /// The number of rows in the window.
    #[must_use]
    pub fn num_rows(&self) -> u32 {
        u32::try_from(self.logtype_words.len()).unwrap_or(u32::MAX)
    }

    /// # Returns
    ///
    /// The logtype id of window row `row` (the low 24 bits of its logtype word).
    ///
    /// # Panics
    ///
    /// Panics if `row` is not below [`Self::num_rows`].
    #[must_use]
    pub fn logtype_id(&self, row: u32) -> LogTypeId {
        LogTypeId::new(self.logtype_words[row as usize].get() & ColumnData::LOGTYPE_ID_MASK)
    }

    /// Returns the logtype of window row `row`.
    ///
    /// # Returns
    ///
    /// The logtype on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::Format`] if the logtype id is out of range.
    ///
    /// # Panics
    ///
    /// Panics if `row` is not below [`Self::num_rows`].
    pub fn logtype(&self, row: u32) -> Result<Logtype<'batch>, TableError> {
        Ok(self.logtypes.get(self.logtype_id(row))?)
    }

    /// Returns the encoded variables of window row `row`.
    ///
    /// # Returns
    ///
    /// The variables on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::VariableOutOfRange`] if the row's variables lie outside the column.
    /// * Forwards [`Self::logtype`]'s return values on failure.
    ///
    /// # Panics
    ///
    /// Panics if `row` is not below [`Self::num_rows`].
    pub fn variables(&self, row: u32) -> Result<&'batch [LeI64], TableError> {
        let logtype = self.logtype(row)?;
        self.variables_of(row, &logtype)
    }

    /// # Returns
    ///
    /// The dictionary the logtype ids refer to (the logtype dictionary for `ClpString` columns, the
    /// array dictionary for unstructured arrays).
    #[must_use]
    pub const fn logtypes(&self) -> &'batch LogTypeDictionary {
        self.logtypes
    }

    /// # Returns
    ///
    /// The variable dictionary that dictionary variables refer to.
    #[must_use]
    pub const fn variable_dictionary(&self) -> &'batch VariableDictionary {
        self.variable_dictionary
    }

    /// Appends the decoded text of window row `row` to `output` (unescaped; for unstructured
    /// arrays, the raw JSON array text), as
    /// `EncodedVariableInterpreter::decode_variables_into_message` reconstructs it: constants are
    /// copied verbatim, an escape placeholder is dropped (keeping the byte it escapes), and each
    /// variable placeholder consumes the next variable. On failure, `output` is left unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::append_decoded`]'s return values on failure.
    ///
    /// # Panics
    ///
    /// Panics if `row` is not below [`Self::num_rows`].
    pub fn decode(&self, row: u32, output: &mut Vec<u8>) -> Result<(), TableError> {
        let original_length = output.len();
        let result = self.append_decoded(row, output);
        if result.is_err() {
            output.truncate(original_length);
        }
        result
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The window `[first_row, first_row + logtype_words.len())` of an encoded-text column of
    /// `table`.
    pub(crate) const fn new(
        logtype_words: &'batch [LeU64],
        variables: &'batch [LeI64],
        logtypes: &'batch LogTypeDictionary,
        variable_dictionary: &'batch VariableDictionary,
        table: TableOrdinal,
        first_row: u32,
    ) -> Self {
        Self {
            logtype_words,
            variables,
            logtypes,
            variable_dictionary,
            table,
            first_row,
        }
    }

    /// # Returns
    ///
    /// The variables of window row `row`, whose logtype is `logtype`, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::VariableOutOfRange`] if the row's variables lie outside the column.
    fn variables_of(&self, row: u32, logtype: &Logtype<'_>) -> Result<&'batch [LeI64], TableError> {
        let num_variables = logtype
            .placeholders
            .iter()
            .filter(|placeholder| VariablePlaceholder::Escape != placeholder.kind)
            .count();
        let offset = self.logtype_words[row as usize].get() >> ColumnData::VARIABLE_OFFSET_SHIFT;
        usize::try_from(offset)
            .ok()
            .and_then(|begin| self.variables.get(begin..begin.checked_add(num_variables)?))
            .ok_or_else(|| TableError::VariableOutOfRange {
                table: self.table,
                row: self.first_row.saturating_add(row),
            })
    }

    /// Appends the decoded text of window row `row` to `output`, leaving partial output behind on
    /// failure.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::Format`] if a variable id is out of range.
    /// * [`TableError::Encoding`] if an encoded float variable is invalid.
    /// * Forwards [`Self::logtype`]'s return values on failure.
    /// * Forwards [`Self::variables_of`]'s return values on failure.
    fn append_decoded(&self, row: u32, output: &mut Vec<u8>) -> Result<(), TableError> {
        let logtype = self.logtype(row)?;
        let mut variables = self.variables_of(row, &logtype)?.iter();
        let text = logtype.text;
        let mut constant_begin = 0;
        for placeholder in logtype.placeholders {
            let offset = placeholder.offset as usize;
            output.extend_from_slice(&text[constant_begin..offset]);
            constant_begin = offset + 1;
            if VariablePlaceholder::Escape == placeholder.kind {
                continue;
            }
            let variable = variables
                .next()
                .expect("`variables_of` returns one variable per variable placeholder")
                .get();
            match placeholder.kind {
                VariablePlaceholder::Integer => decode_integer_variable(variable, output),
                VariablePlaceholder::Float => decode_float_variable(variable, output)?,
                VariablePlaceholder::Dictionary => {
                    let id = VariableId::new(decode_dictionary_variable(variable));
                    output.extend_from_slice(self.variable_dictionary.get(id)?);
                }
                VariablePlaceholder::Escape => {}
            }
        }
        output.extend_from_slice(&text[constant_begin..]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clp_s_encoding::encode_float_variable;
    use clp_s_format::FormatError;
    use clp_s_format::ReaderLimits;

    use super::*;

    const TABLE: TableOrdinal = TableOrdinal::new(3);
    const FIRST_ROW: u32 = 100;

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

    /// The dictionaries and column bytes one encoded-text batch borrows.
    struct Fixture {
        logtypes: LogTypeDictionary,
        variables: VariableDictionary,
        logtype_words: Vec<LeU64>,
        encoded_variables: Vec<LeI64>,
    }

    impl Fixture {
        /// Factory function.
        ///
        /// # Returns
        ///
        /// A column whose rows are `(logtype id, variable offset)` words over `encoded_variables`.
        fn new(
            logtypes: &[&[u8]],
            variables: &[&[u8]],
            rows: &[(u64, u64)],
            encoded_variables: &[i64],
        ) -> Self {
            let limits = ReaderLimits::default();
            Self {
                logtypes: LogTypeDictionary::load(&dictionary_section(logtypes), &limits)
                    .expect("the logtype dictionary loads"),
                variables: VariableDictionary::load(&dictionary_section(variables), &limits)
                    .expect("the variable dictionary loads"),
                logtype_words: rows
                    .iter()
                    .map(|(id, offset)| {
                        LeU64::new(id | offset << ColumnData::VARIABLE_OFFSET_SHIFT)
                    })
                    .collect(),
                encoded_variables: encoded_variables.iter().copied().map(LeI64::new).collect(),
            }
        }

        /// # Returns
        ///
        /// The batch over the fixture's rows.
        fn batch(&self) -> EncodedTextBatch<'_> {
            EncodedTextBatch::new(
                &self.logtype_words,
                &self.encoded_variables,
                &self.logtypes,
                &self.variables,
                TABLE,
                FIRST_ROW,
            )
        }
    }

    /// # Returns
    ///
    /// The decoded text of row `row` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`EncodedTextBatch::decode`]'s return values on failure.
    fn decode(batch: &EncodedTextBatch<'_>, row: u32) -> Result<Vec<u8>, TableError> {
        let mut output = b"prefix|".to_vec();
        batch.decode(row, &mut output)?;
        Ok(output.split_off(7))
    }

    #[test]
    fn decodes_every_variable_kind() -> anyhow::Result<()> {
        let float = encode_float_variable(b"-1.250").expect("the float is encodable");
        let fixture = Fixture::new(
            &[b"int \x11 dict \x12 float \x13 end", b"no variables"],
            &[b"zero", b"user-17"],
            &[(0, 0), (1, 3), (0, 3)],
            &[42, 1, float, -7, 0, float],
        );
        let batch = fixture.batch();
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(batch.logtype_id(1), LogTypeId::new(1));
        assert_eq!(decode(&batch, 0)?, b"int 42 dict user-17 float -1.250 end");
        assert_eq!(decode(&batch, 1)?, b"no variables");
        assert_eq!(decode(&batch, 2)?, b"int -7 dict zero float -1.250 end");
        let variables: Vec<i64> = batch
            .variables(2)?
            .iter()
            .map(|value| value.get())
            .collect();
        assert_eq!(variables, vec![-7, 0, float]);
        assert_eq!(batch.variables(1)?, &[] as &[LeI64]);
        Ok(())
    }

    #[test]
    fn escapes_keep_the_escaped_byte_and_consume_no_variable() -> anyhow::Result<()> {
        let fixture = Fixture::new(&[b"a\\\x11b \x11 c\\\\d\\"], &[], &[(0, 0)], &[5]);
        let batch = fixture.batch();
        assert_eq!(batch.variables(0)?.len(), 1);
        assert_eq!(decode(&batch, 0)?, b"a\x11b 5 c\\d");
        Ok(())
    }

    #[test]
    fn variables_past_the_region_are_out_of_range() {
        let fixture = Fixture::new(&[b"\x11 \x11"], &[], &[(0, 0), (0, 1)], &[1, 2]);
        let batch = fixture.batch();
        for result in [
            batch.variables(1).map(|_| ()),
            decode(&batch, 1).map(|_| ()),
        ] {
            assert!(
                matches!(
                    result,
                    Err(TableError::VariableOutOfRange {
                        table: TABLE,
                        row: 101,
                    })
                ),
                "{result:?}"
            );
        }
    }

    #[test]
    fn unknown_ids_fail_and_leave_the_output_unchanged() {
        let fixture = Fixture::new(&[b"x \x12"], &[b"only"], &[(1, 0), (0, 0)], &[9]);
        let batch = fixture.batch();
        for row in [0, 1] {
            let mut output = b"kept".to_vec();
            let result = batch.decode(row, &mut output);
            assert!(
                matches!(result, Err(TableError::Format(FormatError::Corrupt { .. }))),
                "row {row}: {result:?}"
            );
            assert_eq!(output, b"kept", "row {row}");
        }
    }

    #[test]
    fn invalid_float_variables_are_encoding_errors() {
        let fixture = Fixture::new(&[b"\x13"], &[], &[(0, 0)], &[-1]);
        let result = decode(&fixture.batch(), 0);
        assert!(matches!(result, Err(TableError::Encoding(_))), "{result:?}");
    }

    #[test]
    fn column_values_resolve_dictionary_entries() -> anyhow::Result<()> {
        let fixture = Fixture::new(&[b"text \x11"], &[b"1.50", b"plain"], &[(0, 0)], &[3]);
        let integers = [LeI64::new(-5)];
        let deltas = [9_i64];
        let floats = [LeF64::new(0.25)];
        let formats = [LeU16::new(0x4001)];
        let ids = [LeU64::new(1), LeU64::new(0)];
        let booleans = [0_u8, 2];
        let pattern_ids = [LeU64::new(4)];

        assert!(matches!(
            ColumnBatch::Int64(&integers).value(0)?,
            ValueRef::Integer(-5)
        ));
        assert!(matches!(
            ColumnBatch::DeltaInt64(&deltas).value(0)?,
            ValueRef::Integer(9)
        ));
        assert!(matches!(
            ColumnBatch::Float64(&floats).value(0)?,
            ValueRef::Float(FloatRef::Plain(0.25))
        ));
        assert!(matches!(
            ColumnBatch::FormattedFloat {
                values: &floats,
                formats: &formats,
            }
            .value(0)?,
            ValueRef::Float(FloatRef::Formatted {
                value: 0.25,
                format: 0x4001,
            })
        ));
        assert!(matches!(
            ColumnBatch::DictionaryFloat {
                ids: &ids,
                dictionary: &fixture.variables,
            }
            .value(1)?,
            ValueRef::Float(FloatRef::Dictionary { text: b"1.50", .. })
        ));
        assert!(matches!(
            ColumnBatch::VariableString {
                ids: &ids,
                dictionary: &fixture.variables,
            }
            .value(0)?,
            ValueRef::String(StringRef::Variable { text: b"plain", .. })
        ));
        let booleans = ColumnBatch::Boolean(&booleans);
        assert!(matches!(booleans.value(0)?, ValueRef::Boolean(false)));
        assert!(matches!(booleans.value(1)?, ValueRef::Boolean(true)));
        assert!(matches!(
            ColumnBatch::Timestamp {
                epoch_ns: &deltas,
                pattern_ids: &pattern_ids,
            }
            .value(0)?,
            ValueRef::Timestamp {
                epoch_ns: 9,
                pattern_id,
            } if pattern_id == TimestampPatternId::new(4)
        ));

        let ValueRef::String(StringRef::Encoded(text)) =
            ColumnBatch::ClpString(fixture.batch()).value(0)?
        else {
            panic!("a ClpString value is encoded text");
        };
        let mut decoded = Vec::new();
        text.decode(&mut decoded)?;
        assert_eq!(decoded, b"text 3");
        let ValueRef::UnstructuredArray(array) =
            ColumnBatch::UnstructuredArray(fixture.batch()).value(0)?
        else {
            panic!("an array value is encoded text");
        };
        assert_eq!(array.row(), 0);
        Ok(())
    }

    #[test]
    fn unknown_dictionary_ids_fail_value_resolution() {
        let fixture = Fixture::new(&[], &[b"one"], &[], &[]);
        let ids = [LeU64::new(1)];
        let result = ColumnBatch::VariableString {
            ids: &ids,
            dictionary: &fixture.variables,
        }
        .value(0);
        assert!(
            matches!(result, Err(TableError::Format(FormatError::Corrupt { .. }))),
            "{result:?}"
        );
    }
}
