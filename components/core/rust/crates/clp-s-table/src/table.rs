//! Loaded tables.

use std::ops::Range;
use std::sync::Arc;

use clp_s_format::DictionaryKind;
use clp_s_format::LogTypeDictionary;
use clp_s_format::TableBytes;
use clp_s_format::TableInfo;
use clp_s_format::TableOrdinal;
use clp_s_format::VariableDictionary;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::KnownLayout;

use crate::ArchiveTables;
use crate::ColumnBatch;
use crate::ColumnEncoding;
use crate::ColumnIndex;
use crate::EncodedTextBatch;
use crate::LeI64;
use crate::TableError;
use crate::TableLayout;

/// One loaded ERT: shared decoded bytes plus validated column offsets. Immutable and `Sync`.
#[derive(Debug)]
pub struct Table {
    archive: Arc<ArchiveTables>,
    layout: Arc<TableLayout>,
    bytes: TableBytes,

    /// One entry per layout column, in buffer order.
    columns: Box<[ColumnData]>,
}

impl Table {
    /// Factory function.
    ///
    /// Checks that the layout's columns exactly tile `bytes` (in O(columns)), and that every
    /// per-row variable offset of the `ClpString` and unstructured-array columns lies inside its
    /// column (in O(rows) per such column; the variable counts live in the dictionaries, so the
    /// full range of a row is checked when its variables are read).
    ///
    /// # Returns
    ///
    /// The loaded table on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`ArchiveTables::layout`]'s return values on failure.
    /// * Forwards [`ColumnData::tile`]'s return values on failure.
    pub fn load(archive: Arc<ArchiveTables>, bytes: TableBytes) -> Result<Self, TableError> {
        let info = *bytes.info();
        let layout = archive.layout(info.ordinal)?;
        let columns = ColumnData::tile(
            layout.columns().iter().map(|spec| spec.encoding),
            bytes.as_bytes(),
            info.num_rows,
            info.ordinal,
        )?;
        Ok(Self {
            archive,
            layout,
            bytes,
            columns,
        })
    }

    /// # Returns
    ///
    /// The table's directory entry.
    #[must_use]
    pub const fn info(&self) -> &TableInfo {
        self.bytes.info()
    }

    /// # Returns
    ///
    /// The table's layout.
    #[must_use]
    pub const fn layout(&self) -> &Arc<TableLayout> {
        &self.layout
    }

    /// # Returns
    ///
    /// The archive the table belongs to.
    #[must_use]
    pub const fn archive(&self) -> &Arc<ArchiveTables> {
        &self.archive
    }

    /// # Returns
    ///
    /// The number of rows.
    #[must_use]
    pub const fn num_rows(&self) -> u32 {
        self.bytes.info().num_rows
    }

    /// # Returns
    ///
    /// The ERT bytes.
    pub(crate) fn bytes(&self) -> &[u8] {
        self.bytes.as_bytes()
    }

    /// # Returns
    ///
    /// The validated byte ranges of `column`, or `None` if it is outside the layout.
    pub(crate) fn column_data(&self, column: ColumnIndex) -> Option<&ColumnData> {
        self.columns.get(column.get() as usize)
    }

    /// Restricts one column to the table rows `rows`.
    ///
    /// # Returns
    ///
    /// The column on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::ColumnNotDecoded`] if `column` is outside the layout, or if it is
    ///   delta-encoded and `decoded` holds no window of `rows.len()` rows for it (callers check
    ///   that the column is decoded in this window, so this indicates an internal inconsistency).
    /// * [`TableError::DictionaryNotLoaded`] if the column's dictionary was not loaded.
    /// * [`TableError::ColumnTiling`] if the column's byte ranges do not match its encoding (both
    ///   come from the same layout, so this indicates an internal inconsistency).
    /// * Forwards [`window_slice`]'s return values on failure.
    /// * Forwards [`cast_slice`]'s return values on failure.
    pub(crate) fn column_batch<'batch>(
        &'batch self,
        decoded: &'batch [Vec<i64>],
        column: ColumnIndex,
        rows: Range<u32>,
    ) -> Result<ColumnBatch<'batch>, TableError> {
        let (Some(spec), Some(data)) = (self.layout().column(column), self.column_data(column))
        else {
            return Err(TableError::ColumnNotDecoded(column));
        };
        let num_rows = rows.len();
        let decoded_window = || {
            decoded
                .get(column.get() as usize)
                .map(Vec::as_slice)
                .filter(|values| values.len() == num_rows)
                .ok_or(TableError::ColumnNotDecoded(column))
        };
        let dictionaries = self.archive().dictionaries();
        let variables = || -> Result<&'batch VariableDictionary, TableError> {
            dictionaries
                .variable
                .get()
                .map(Arc::as_ref)
                .ok_or(TableError::DictionaryNotLoaded(DictionaryKind::Variable))
        };
        let bytes = self.bytes();
        let ordinal = self.info().ordinal;
        Ok(match (spec.encoding, data) {
            (ColumnEncoding::Int64, ColumnData::Values(range)) => {
                ColumnBatch::Int64(window_slice(bytes, range, rows, ordinal)?)
            }
            (ColumnEncoding::DeltaInt64, ColumnData::Values(_)) => {
                ColumnBatch::DeltaInt64(decoded_window()?)
            }
            (ColumnEncoding::Float64, ColumnData::Values(range)) => {
                ColumnBatch::Float64(window_slice(bytes, range, rows, ordinal)?)
            }
            (ColumnEncoding::FormattedFloat64, ColumnData::FormattedFloat { values, formats }) => {
                ColumnBatch::FormattedFloat {
                    values: window_slice(bytes, values, rows.clone(), ordinal)?,
                    formats: window_slice(bytes, formats, rows, ordinal)?,
                }
            }
            (ColumnEncoding::DictionaryFloat, ColumnData::Values(range)) => {
                ColumnBatch::DictionaryFloat {
                    ids: window_slice(bytes, range, rows, ordinal)?,
                    dictionary: variables()?,
                }
            }
            (ColumnEncoding::Boolean, ColumnData::Values(range)) => {
                ColumnBatch::Boolean(window_slice(bytes, range, rows, ordinal)?)
            }
            (ColumnEncoding::VariableString, ColumnData::Values(range)) => {
                ColumnBatch::VariableString {
                    ids: window_slice(bytes, range, rows, ordinal)?,
                    dictionary: variables()?,
                }
            }
            (
                encoding @ (ColumnEncoding::ClpString | ColumnEncoding::UnstructuredArray),
                ColumnData::EncodedText {
                    logtypes,
                    variables: variable_range,
                },
            ) => {
                let (cell, kind) = if ColumnEncoding::ClpString == encoding {
                    (&dictionaries.logtype, DictionaryKind::LogType)
                } else {
                    (&dictionaries.array, DictionaryKind::Array)
                };
                let logtype_dictionary: &'batch LogTypeDictionary = cell
                    .get()
                    .map(Arc::as_ref)
                    .ok_or(TableError::DictionaryNotLoaded(kind))?;
                let first_row = rows.start;
                let batch = EncodedTextBatch::new(
                    window_slice(bytes, logtypes, rows, ordinal)?,
                    cast_slice(&bytes[variable_range.clone()], ordinal)?,
                    logtype_dictionary,
                    variables()?,
                    ordinal,
                    first_row,
                );
                if ColumnEncoding::ClpString == encoding {
                    ColumnBatch::ClpString(batch)
                } else {
                    ColumnBatch::UnstructuredArray(batch)
                }
            }
            (
                ColumnEncoding::Timestamp,
                ColumnData::Timestamp {
                    pattern_ids: range, ..
                },
            ) => ColumnBatch::Timestamp {
                epoch_ns: decoded_window()?,
                pattern_ids: window_slice(bytes, range, rows, ordinal)?,
            },
            _ => return Err(TableError::ColumnTiling(ordinal)),
        })
    }

    /// Returns the raw deltas of the delta-encoded column `column` for the table rows `rows`.
    ///
    /// # Returns
    ///
    /// The deltas on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::ColumnNotDecoded`] if `column` is not a delta-encoded column of the layout.
    /// * Forwards [`window_slice`]'s return values on failure.
    pub(crate) fn delta_window(
        &self,
        column: ColumnIndex,
        rows: Range<u32>,
    ) -> Result<&[LeI64], TableError> {
        match self.column_data(column) {
            Some(
                ColumnData::Values(range)
                | ColumnData::Timestamp {
                    epoch_ns: range, ..
                },
            ) => window_slice(self.bytes(), range, rows, self.info().ordinal),
            _ => Err(TableError::ColumnNotDecoded(column)),
        }
    }
}

/// The validated byte ranges of one column inside its ERT.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnData {
    /// One fixed-width value per row.
    Values(Range<usize>),

    /// `f64` values, then one `u16` format per row.
    FormattedFloat {
        values: Range<usize>,
        formats: Range<usize>,
    },

    /// `u64` logtype words, then the `u64` variable count (not part of either range), then the
    /// `i64` variables.
    EncodedText {
        logtypes: Range<usize>,
        variables: Range<usize>,
    },

    /// `i64` epoch deltas, then one `u64` pattern id per row.
    Timestamp {
        epoch_ns: Range<usize>,
        pattern_ids: Range<usize>,
    },
}

impl ColumnData {
    /// Mask of the logtype id inside a logtype word (`ClpStringColumnWriter::cLogDictIdMask`).
    pub(crate) const LOGTYPE_ID_MASK: u64 = (1 << Self::VARIABLE_OFFSET_SHIFT) - 1;
    /// Bit position of the variable offset inside a logtype word
    /// (`ClpStringColumnWriter::cOffsetBitPosition`).
    pub(crate) const VARIABLE_OFFSET_SHIFT: u32 = 24;

    /// Splits the bytes of an ERT into its columns, in buffer order, as the C++ column readers
    /// load them (`ColumnReader.cpp`): every column holds `num_rows` values back to back, and an
    /// encoded-text column's variable region is prefixed with its `u64` length.
    ///
    /// # Type Parameters
    ///
    /// * `EncodingsType` - Iterates over the encodings of the columns, in buffer order.
    ///
    /// # Returns
    ///
    /// The byte ranges of each column on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::ColumnTiling`] if the columns overrun `bytes` or leave bytes unused.
    /// * Forwards [`check_variable_offsets`]'s return values on failure (only once the columns tile
    ///   `bytes`).
    pub(crate) fn tile<EncodingsType: Iterator<Item = ColumnEncoding>>(
        encodings: EncodingsType,
        bytes: &[u8],
        num_rows: u32,
        table: TableOrdinal,
    ) -> Result<Box<[Self]>, TableError> {
        let mut cursor = Cursor {
            bytes,
            position: 0,
            table,
        };
        let num_rows_u64 = u64::from(num_rows);
        let mut columns = Vec::new();
        let mut variable_offsets = Vec::new();
        for encoding in encodings {
            let column = match encoding {
                ColumnEncoding::Int64
                | ColumnEncoding::DeltaInt64
                | ColumnEncoding::Float64
                | ColumnEncoding::DictionaryFloat
                | ColumnEncoding::VariableString => Self::Values(cursor.take(num_rows_u64, 8)?),
                ColumnEncoding::Boolean => Self::Values(cursor.take(num_rows_u64, 1)?),
                ColumnEncoding::FormattedFloat64 => Self::FormattedFloat {
                    values: cursor.take(num_rows_u64, 8)?,
                    formats: cursor.take(num_rows_u64, 2)?,
                },
                ColumnEncoding::ClpString | ColumnEncoding::UnstructuredArray => {
                    let logtypes = cursor.take(num_rows_u64, 8)?;
                    let num_variables = cursor.read_u64()?;
                    let variables = cursor.take(num_variables, 8)?;
                    variable_offsets.push((logtypes.clone(), num_variables));
                    Self::EncodedText {
                        logtypes,
                        variables,
                    }
                }
                ColumnEncoding::Timestamp => Self::Timestamp {
                    epoch_ns: cursor.take(num_rows_u64, 8)?,
                    pattern_ids: cursor.take(num_rows_u64, 8)?,
                },
            };
            columns.push(column);
        }
        if cursor.position != bytes.len() {
            return Err(TableError::ColumnTiling(table));
        }
        for (logtypes, num_variables) in variable_offsets {
            check_variable_offsets(&bytes[logtypes], num_variables, table)?;
        }
        Ok(columns.into_boxed_slice())
    }
}

/// A bounds-checked reader over the bytes of one ERT.
struct Cursor<'bytes> {
    bytes: &'bytes [u8],
    position: usize,
    table: TableOrdinal,
}

impl Cursor<'_> {
    /// Consumes `count` elements of `element_size` bytes.
    ///
    /// # Returns
    ///
    /// The consumed range on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::ColumnTiling`] if fewer bytes remain.
    fn take(&mut self, count: u64, element_size: u64) -> Result<Range<usize>, TableError> {
        let end = count
            .checked_mul(element_size)
            .and_then(|size| usize::try_from(size).ok())
            .and_then(|size| self.position.checked_add(size))
            .filter(|end| *end <= self.bytes.len())
            .ok_or(TableError::ColumnTiling(self.table))?;
        let range = self.position..end;
        self.position = end;
        Ok(range)
    }

    /// Consumes one little-endian `u64`.
    ///
    /// # Returns
    ///
    /// The value on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::take`]'s return values on failure.
    fn read_u64(&mut self) -> Result<u64, TableError> {
        let range = self.take(1, 8)?;
        let mut value = [0; 8];
        value.copy_from_slice(&self.bytes[range]);
        Ok(u64::from_le_bytes(value))
    }
}

/// Checks that the variable offset of every logtype word in `logtypes` is at most
/// `num_variables`.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`TableError::VariableOutOfRange`] if an offset lies past the variable region.
fn check_variable_offsets(
    logtypes: &[u8],
    num_variables: u64,
    table: TableOrdinal,
) -> Result<(), TableError> {
    for (row, word) in (0_u32..).zip(logtypes.chunks_exact(8)) {
        let mut value = [0; 8];
        value.copy_from_slice(word);
        if u64::from_le_bytes(value) >> ColumnData::VARIABLE_OFFSET_SHIFT > num_variables {
            return Err(TableError::VariableOutOfRange { table, row });
        }
    }
    Ok(())
}

/// Restricts a column region of fixed-width elements to the rows `rows`.
///
/// # Type Parameters
///
/// * `ElementType` - The alignment-1 element type of the region.
///
/// # Returns
///
/// The elements of the rows on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`TableError::ColumnTiling`] if the rows lie outside the region.
/// * Forwards [`cast_slice`]'s return values on failure.
fn window_slice<'bytes, ElementType: FromBytes + Immutable + KnownLayout>(
    bytes: &'bytes [u8],
    region: &Range<usize>,
    rows: Range<u32>,
    table: TableOrdinal,
) -> Result<&'bytes [ElementType], TableError> {
    let element_size = size_of::<ElementType>();
    let begin = region.start + rows.start as usize * element_size;
    let end = region.start + rows.end as usize * element_size;
    if end > region.end {
        return Err(TableError::ColumnTiling(table));
    }
    cast_slice(&bytes[begin..end], table)
}

/// Views `bytes` as a slice of alignment-1 elements.
///
/// # Type Parameters
///
/// * `ElementType` - The alignment-1 element type.
///
/// # Returns
///
/// The elements on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`TableError::ColumnTiling`] if `bytes` is not a whole number of aligned elements (column
///   regions are validated at load, so this indicates an internal inconsistency).
fn cast_slice<ElementType: FromBytes + Immutable + KnownLayout>(
    bytes: &[u8],
    table: TableOrdinal,
) -> Result<&[ElementType], TableError> {
    <[ElementType]>::ref_from_bytes(bytes).map_err(|_| TableError::ColumnTiling(table))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: TableOrdinal = TableOrdinal::new(7);

    /// # Returns
    ///
    /// The ranges of `encodings` over `bytes` with `num_rows` rows on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`ColumnData::tile`]'s return values on failure.
    fn tile(
        encodings: &[ColumnEncoding],
        bytes: &[u8],
        num_rows: u32,
    ) -> Result<Box<[ColumnData]>, TableError> {
        ColumnData::tile(encodings.iter().copied(), bytes, num_rows, TABLE)
    }

    /// # Returns
    ///
    /// A logtype word for logtype `id` whose variables start at `offset`.
    const fn logtype_word(id: u64, offset: u64) -> u64 {
        id | offset << ColumnData::VARIABLE_OFFSET_SHIFT
    }

    /// # Returns
    ///
    /// The little-endian bytes of `values`.
    fn le_bytes(values: &[u64]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    #[test]
    fn fixed_width_columns_tile_in_buffer_order() -> anyhow::Result<()> {
        let encodings = [
            ColumnEncoding::Int64,
            ColumnEncoding::Boolean,
            ColumnEncoding::FormattedFloat64,
            ColumnEncoding::Timestamp,
            ColumnEncoding::DeltaInt64,
            ColumnEncoding::Float64,
            ColumnEncoding::DictionaryFloat,
            ColumnEncoding::VariableString,
        ];
        let num_rows = 3;
        let bytes = vec![0; 3 * (8 + 1 + 10 + 16 + 8 + 8 + 8 + 8)];
        let columns = tile(&encodings, &bytes, num_rows)?;
        assert_eq!(
            &*columns,
            &[
                ColumnData::Values(0..24),
                ColumnData::Values(24..27),
                ColumnData::FormattedFloat {
                    values: 27..51,
                    formats: 51..57,
                },
                ColumnData::Timestamp {
                    epoch_ns: 57..81,
                    pattern_ids: 81..105,
                },
                ColumnData::Values(105..129),
                ColumnData::Values(129..153),
                ColumnData::Values(153..177),
                ColumnData::Values(177..201),
            ]
        );
        Ok(())
    }

    #[test]
    fn encoded_text_columns_read_their_variable_count() -> anyhow::Result<()> {
        let mut bytes = le_bytes(&[logtype_word(5, 0), logtype_word(6, 2), 3]);
        bytes.extend(le_bytes(&[10, 20, 30]));
        bytes.extend([1, 0]);
        let columns = tile(
            &[ColumnEncoding::ClpString, ColumnEncoding::Boolean],
            &bytes,
            2,
        )?;
        assert_eq!(
            &*columns,
            &[
                ColumnData::EncodedText {
                    logtypes: 0..16,
                    variables: 24..48,
                },
                ColumnData::Values(48..50),
            ]
        );
        Ok(())
    }

    #[test]
    fn variable_offsets_may_end_at_the_region() -> anyhow::Result<()> {
        let bytes = le_bytes(&[logtype_word(1, 0), logtype_word(1, 1), 1, 99]);
        tile(&[ColumnEncoding::UnstructuredArray], &bytes, 2)?;
        Ok(())
    }

    #[test]
    fn variable_offsets_past_the_region_are_rejected() {
        let bytes = le_bytes(&[logtype_word(1, 0), logtype_word(1, 2), 1, 99]);
        let result = tile(&[ColumnEncoding::ClpString], &bytes, 2);
        assert!(
            matches!(
                result,
                Err(TableError::VariableOutOfRange {
                    table: TABLE,
                    row: 1
                })
            ),
            "{result:?}"
        );
    }

    #[test]
    fn short_or_long_bytes_do_not_tile() {
        let encodings = [ColumnEncoding::Int64, ColumnEncoding::Boolean];
        for size in [0, 16, 17, 19, 1000] {
            let result = tile(&encodings, &vec![0; size], 2);
            assert!(
                matches!(result, Err(TableError::ColumnTiling(TABLE))),
                "{size} bytes: {result:?}"
            );
        }
    }

    #[test]
    fn huge_variable_counts_do_not_tile() {
        for num_variables in [u64::MAX, u64::MAX / 8 + 1, 2] {
            let bytes = le_bytes(&[logtype_word(1, 0), num_variables, 0]);
            let result = tile(&[ColumnEncoding::ClpString], &bytes, 1);
            assert!(
                matches!(result, Err(TableError::ColumnTiling(TABLE))),
                "{num_variables} variables: {result:?}"
            );
        }
    }

    #[test]
    fn empty_tables_tile_with_empty_columns() -> anyhow::Result<()> {
        let bytes = le_bytes(&[0]);
        let columns = tile(
            &[ColumnEncoding::Int64, ColumnEncoding::ClpString],
            &bytes,
            0,
        )?;
        assert_eq!(
            &*columns,
            &[
                ColumnData::Values(0..0),
                ColumnData::EncodedText {
                    logtypes: 0..0,
                    variables: 8..8,
                },
            ]
        );
        Ok(())
    }

    #[test]
    fn window_slices_stay_inside_their_region() -> anyhow::Result<()> {
        let bytes: Vec<u8> = (0_u8..32).collect();
        let table = TableOrdinal::new(0);
        let slice: &[LeI64] = window_slice(&bytes, &(8..32), 1..3, table)?;
        assert_eq!(
            slice.iter().map(|value| value.get()).collect::<Vec<_>>(),
            vec![
                i64::from_le_bytes(bytes[16..24].try_into()?),
                i64::from_le_bytes(bytes[24..32].try_into()?),
            ]
        );
        let result: Result<&[LeI64], _> = window_slice(&bytes, &(8..32), 2..4, table);
        assert!(
            matches!(result, Err(TableError::ColumnTiling(_))),
            "{result:?}"
        );
        Ok(())
    }
}
