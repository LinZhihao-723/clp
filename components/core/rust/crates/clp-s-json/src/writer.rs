//! Stateless value writers: one closed enum variant per column encoding, bound to the column
//! slice of one window.

use clp_s_format::TimestampIndex;
use clp_s_format::TimestampPatternId;
use clp_s_format::VariableDictionary;
use clp_s_format::VariableId;
use clp_s_table::ColumnBatch;
use clp_s_table::ColumnEncoding;
use clp_s_table::EncodedTextBatch;
use clp_s_table::LeF64;
use clp_s_table::LeI64;
use clp_s_table::LeU16;
use clp_s_table::LeU64;

use crate::JsonError;
use crate::escape::escape_into;
use crate::float::write_cpp_plain;
use crate::float::write_formatted;
use crate::float::write_shortest;
use crate::logtype::LogtypeCache;

/// The writer of one value slot, bound to its column's window.
#[derive(Clone, Copy, Debug)]
pub enum ValueWriter<'batch> {
    Int64(&'batch [LeI64]),
    DeltaInt64(&'batch [i64]),
    ShortestFloat(&'batch [LeF64]),
    CppPlainFloat(&'batch [LeF64]),
    FormattedFloat {
        values: &'batch [LeF64],
        formats: &'batch [LeU16],
    },
    DictionaryFloat {
        ids: &'batch [LeU64],
        dictionary: &'batch VariableDictionary,
    },
    Boolean(&'batch [u8]),
    ClpString {
        batch: EncodedTextBatch<'batch>,
        logtypes: &'batch LogtypeCache,
    },
    VariableString {
        ids: &'batch [LeU64],
        dictionary: &'batch VariableDictionary,
    },
    UnstructuredArray(EncodedTextBatch<'batch>),
    Timestamp {
        epoch_ns: &'batch [i64],
        pattern_ids: &'batch [LeU64],
        timestamps: &'batch TimestampIndex,
    },
}

impl<'batch> ValueWriter<'batch> {
    /// Binds the writer of a slot that expects `encoding` to its column window.
    ///
    /// # Returns
    ///
    /// The writer, or `None` if `column` does not hold `encoding` values.
    pub fn bind(
        encoding: ColumnEncoding,
        column: ColumnBatch<'batch>,
        should_emulate_cpp_plain_floats: bool,
        logtypes: &'batch LogtypeCache,
        timestamps: &'batch TimestampIndex,
    ) -> Option<Self> {
        let writer = match column {
            ColumnBatch::Int64(values) => Self::Int64(values),
            ColumnBatch::DeltaInt64(values) => Self::DeltaInt64(values),
            ColumnBatch::Float64(values) if should_emulate_cpp_plain_floats => {
                Self::CppPlainFloat(values)
            }
            ColumnBatch::Float64(values) => Self::ShortestFloat(values),
            ColumnBatch::FormattedFloat { values, formats } => {
                Self::FormattedFloat { values, formats }
            }
            ColumnBatch::DictionaryFloat { ids, dictionary } => {
                Self::DictionaryFloat { ids, dictionary }
            }
            ColumnBatch::Boolean(values) => Self::Boolean(values),
            ColumnBatch::ClpString(batch) => Self::ClpString { batch, logtypes },
            ColumnBatch::VariableString { ids, dictionary } => {
                Self::VariableString { ids, dictionary }
            }
            ColumnBatch::UnstructuredArray(batch) => Self::UnstructuredArray(batch),
            ColumnBatch::Timestamp {
                epoch_ns,
                pattern_ids,
            } => Self::Timestamp {
                epoch_ns,
                pattern_ids,
                timestamps,
            },
            _ => return None,
        };
        (writer.encoding() == encoding).then_some(writer)
    }

    /// Appends the value of window row `row` to `output`: the JSON literal, except that strings
    /// are written escaped without their quotes (the template holds them).
    ///
    /// On failure, `output` may hold a partial value.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`JsonError::Table`] if a dictionary entry, logtype or variable cannot be decoded.
    /// * [`JsonError::Format`] if a variable-dictionary id or timestamp pattern id is out of range,
    ///   or the pattern cannot be compiled (DV-32).
    /// * [`JsonError::Timestamp`] if the timestamp cannot be formatted with its pattern.
    ///
    /// # Panics
    ///
    /// Panics if `row` is not a row of the window.
    pub fn write(&self, row: u32, output: &mut Vec<u8>) -> Result<(), JsonError> {
        let index = row as usize;
        match *self {
            Self::Int64(values) => {
                output
                    .extend_from_slice(itoa::Buffer::new().format(values[index].get()).as_bytes());
            }
            Self::DeltaInt64(values) => {
                output.extend_from_slice(itoa::Buffer::new().format(values[index]).as_bytes());
            }
            Self::ShortestFloat(values) => write_shortest(values[index].get(), output),
            Self::CppPlainFloat(values) => write_cpp_plain(values[index].get(), output),
            Self::FormattedFloat { values, formats } => {
                write_formatted(values[index].get(), formats[index].get(), output);
            }
            Self::DictionaryFloat { ids, dictionary } => {
                output.extend_from_slice(dictionary.get(VariableId::new(ids[index].get()))?);
            }
            Self::Boolean(values) => {
                output.extend_from_slice(if 0 == values[index] {
                    b"false"
                } else {
                    b"true"
                });
            }
            Self::ClpString { batch, logtypes } => {
                let variables = batch.variables(row)?;
                logtypes.write(
                    batch.logtypes(),
                    batch.logtype_id(row),
                    variables,
                    batch.variable_dictionary(),
                    output,
                )?;
            }
            Self::VariableString { ids, dictionary } => {
                escape_into(dictionary.get(VariableId::new(ids[index].get()))?, output);
            }
            Self::UnstructuredArray(batch) => batch.decode(row, output)?,
            Self::Timestamp {
                epoch_ns,
                pattern_ids,
                timestamps,
            } => {
                timestamps
                    .pattern(TimestampPatternId::new(pattern_ids[index].get()))?
                    .format_json(epoch_ns[index], output)?;
            }
        }
        Ok(())
    }

    /// # Returns
    ///
    /// The column encoding the writer reads.
    const fn encoding(&self) -> ColumnEncoding {
        match self {
            Self::Int64(_) => ColumnEncoding::Int64,
            Self::DeltaInt64(_) => ColumnEncoding::DeltaInt64,
            Self::ShortestFloat(_) | Self::CppPlainFloat(_) => ColumnEncoding::Float64,
            Self::FormattedFloat { .. } => ColumnEncoding::FormattedFloat64,
            Self::DictionaryFloat { .. } => ColumnEncoding::DictionaryFloat,
            Self::Boolean(_) => ColumnEncoding::Boolean,
            Self::ClpString { .. } => ColumnEncoding::ClpString,
            Self::VariableString { .. } => ColumnEncoding::VariableString,
            Self::UnstructuredArray(_) => ColumnEncoding::UnstructuredArray,
            Self::Timestamp { .. } => ColumnEncoding::Timestamp,
        }
    }
}
