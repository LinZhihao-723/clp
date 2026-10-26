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
use crate::LeadingColumnsSize;
use crate::TableError;
use crate::TableLayout;

/// One loaded ERT: shared decoded bytes plus validated column offsets. Immutable and `Sync`.
#[derive(Debug)]
pub struct Table {
    archive: Arc<ArchiveTables>,
    layout: Arc<TableLayout>,
    bytes: TableBytes,

    /// The leading columns the bytes hold: every layout column unless the table was loaded from
    /// a prefix of its bytes.
    loaded: LoadedColumns,
}

impl Table {
    /// Factory function.
    ///
    /// Checks that the layout's columns exactly tile `bytes` (in O(columns)), and that every
    /// per-row variable offset of the `ClpString` and unstructured-array columns lies inside its
    /// column (in O(rows) per such column; the variable counts live in the dictionaries, so the
    /// full range of a row is checked when its variables are read). If `bytes` holds only a prefix
    /// of the table, the leading columns that lie wholly inside it are loaded and checked.
    ///
    /// # Returns
    ///
    /// The loaded table on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::load_after`]'s return values on failure.
    pub fn load(archive: Arc<ArchiveTables>, bytes: TableBytes) -> Result<Self, TableError> {
        Self::load_after(archive, bytes, LoadedColumns::default())
    }

    /// Factory function.
    ///
    /// Loads a table like [`Self::load`] from more of its bytes, after `loaded`, the leading
    /// columns of an earlier load of a prefix of the same bytes, which are not checked again.
    ///
    /// # Returns
    ///
    /// The loaded table on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::ColumnTiling`] if `loaded` holds more columns or bytes than the table.
    /// * Forwards [`ArchiveTables::layout`]'s return values on failure.
    /// * Forwards [`ColumnData::tile`]'s return values on failure.
    pub fn load_after(
        archive: Arc<ArchiveTables>,
        bytes: TableBytes,
        loaded: LoadedColumns,
    ) -> Result<Self, TableError> {
        let info = *bytes.info();
        let layout = archive.layout(info.ordinal)?;
        if loaded.columns.len() > layout.columns().len() || loaded.size > bytes.as_bytes().len() {
            return Err(TableError::ColumnTiling(info.ordinal));
        }
        let loaded = ColumnData::tile(
            layout.columns().iter().map(|spec| spec.encoding),
            bytes.as_bytes(),
            bytes.is_complete(),
            info.num_rows,
            info.ordinal,
            loaded,
        )?;
        Ok(Self {
            archive,
            layout,
            bytes,
            loaded,
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
    /// Whether the bytes of `column` are loaded.
    #[must_use]
    pub const fn is_column_loaded(&self, column: ColumnIndex) -> bool {
        (column.get() as usize) < self.loaded.columns.len()
    }

    /// # Returns
    ///
    /// The checked leading columns, for [`Self::load_after`].
    #[must_use]
    pub fn loaded_columns(&self) -> LoadedColumns {
        self.loaded.clone()
    }

    /// # Returns
    ///
    /// The validated byte ranges of `column`, or `None` if it is outside the layout or not loaded.
    pub(crate) fn column_data(&self, column: ColumnIndex) -> Option<&ColumnData> {
        self.loaded.columns.get(column.get() as usize)
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

/// The checked leading columns of a table loaded from a prefix of its bytes.
#[derive(Clone, Debug, Default)]
pub struct LoadedColumns {
    columns: Vec<ColumnData>,

    /// The end of the last column.
    size: usize,
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
    /// Tiling continues after the columns of `loaded`. If `bytes` holds the whole table
    /// (`is_complete`), the columns must tile it exactly; otherwise the leading columns that lie
    /// wholly inside it are tiled.
    ///
    /// # Type Parameters
    ///
    /// * `EncodingsType` - Iterates over the encodings of the columns, in buffer order.
    ///
    /// # Returns
    ///
    /// The tiled columns on success.
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
        is_complete: bool,
        num_rows: u32,
        table: TableOrdinal,
        loaded: LoadedColumns,
    ) -> Result<LoadedColumns, TableError> {
        let LoadedColumns { mut columns, size } = loaded;
        let mut cursor = Cursor {
            bytes,
            position: size,
            table,
            num_variables: 0,
        };
        let num_rows_u64 = u64::from(num_rows);
        let mut variable_offsets = Vec::new();
        let mut is_truncated = false;
        for encoding in encodings.skip(columns.len()) {
            let begin = cursor.position;
            let column = match Self::tile_column(&mut cursor, encoding, num_rows_u64) {
                Ok(column) => column,
                Err(_) if !is_complete => {
                    cursor.position = begin;
                    is_truncated = true;
                    break;
                }
                Err(error) => return Err(error),
            };
            if let Self::EncodedText { logtypes, .. } = &column {
                variable_offsets.push((logtypes.clone(), cursor.num_variables));
            }
            columns.push(column);
        }
        let is_tiled = is_truncated || (is_complete && cursor.position == bytes.len());
        if !is_tiled {
            return Err(TableError::ColumnTiling(table));
        }
        for (logtypes, num_variables) in variable_offsets {
            check_variable_offsets(&bytes[logtypes], num_variables, table)?;
        }
        Ok(LoadedColumns {
            columns,
            size: cursor.position,
        })
    }

    /// Measures the leading `num_columns` columns of an ERT from `prefix`, its decoded leading
    /// bytes, which must reach the variable count of every encoded-text column measured.
    ///
    /// # Type Parameters
    ///
    /// * `EncodingsType` - Iterates over the encodings of the columns, in buffer order.
    ///
    /// # Returns
    ///
    /// The size of the columns on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::ColumnTiling`] if the columns overrun `table_size` bytes.
    pub(crate) fn measure<EncodingsType: Iterator<Item = ColumnEncoding>>(
        encodings: EncodingsType,
        prefix: &[u8],
        num_rows: u32,
        table_size: u64,
        table: TableOrdinal,
    ) -> Result<LeadingColumnsSize, TableError> {
        let num_rows = u64::from(num_rows);
        let overrun = || TableError::ColumnTiling(table);
        let mut position: u64 = 0;
        for encoding in encodings {
            let size = match encoding {
                ColumnEncoding::Int64
                | ColumnEncoding::DeltaInt64
                | ColumnEncoding::Float64
                | ColumnEncoding::DictionaryFloat
                | ColumnEncoding::VariableString => num_rows.checked_mul(8),
                ColumnEncoding::Boolean => Some(num_rows),
                ColumnEncoding::FormattedFloat64 => num_rows.checked_mul(8 + 2),
                ColumnEncoding::Timestamp => num_rows.checked_mul(8 + 8),
                ColumnEncoding::ClpString | ColumnEncoding::UnstructuredArray => {
                    let count_end = num_rows
                        .checked_mul(8)
                        .and_then(|size| position.checked_add(size))
                        .and_then(|logtypes_end| logtypes_end.checked_add(8))
                        .filter(|count_end| *count_end <= table_size)
                        .ok_or_else(overrun)?;
                    let count = usize::try_from(count_end)
                        .ok()
                        .and_then(|end| prefix.get(end - 8..end));
                    let Some(count) = count else {
                        return Ok(LeadingColumnsSize::AtLeast(count_end));
                    };
                    let mut value = [0; 8];
                    value.copy_from_slice(count);
                    u64::from_le_bytes(value)
                        .checked_mul(8)
                        .and_then(|size| size.checked_add(count_end - position))
                }
            };
            position = size
                .and_then(|size| position.checked_add(size))
                .filter(|end| *end <= table_size)
                .ok_or_else(overrun)?;
        }
        Ok(LeadingColumnsSize::Known(position))
    }

    /// Tiles the column of `encoding` at the cursor, leaving an encoded-text column's variable
    /// count in [`Cursor::num_variables`].
    ///
    /// # Returns
    ///
    /// The column on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Cursor::take`]'s return values on failure.
    /// * Forwards [`Cursor::read_u64`]'s return values on failure.
    fn tile_column(
        cursor: &mut Cursor<'_>,
        encoding: ColumnEncoding,
        num_rows: u64,
    ) -> Result<Self, TableError> {
        Ok(match encoding {
            ColumnEncoding::Int64
            | ColumnEncoding::DeltaInt64
            | ColumnEncoding::Float64
            | ColumnEncoding::DictionaryFloat
            | ColumnEncoding::VariableString => Self::Values(cursor.take(num_rows, 8)?),
            ColumnEncoding::Boolean => Self::Values(cursor.take(num_rows, 1)?),
            ColumnEncoding::FormattedFloat64 => Self::FormattedFloat {
                values: cursor.take(num_rows, 8)?,
                formats: cursor.take(num_rows, 2)?,
            },
            ColumnEncoding::ClpString | ColumnEncoding::UnstructuredArray => {
                let logtypes = cursor.take(num_rows, 8)?;
                cursor.num_variables = cursor.read_u64()?;
                let variables = cursor.take(cursor.num_variables, 8)?;
                Self::EncodedText {
                    logtypes,
                    variables,
                }
            }
            ColumnEncoding::Timestamp => Self::Timestamp {
                epoch_ns: cursor.take(num_rows, 8)?,
                pattern_ids: cursor.take(num_rows, 8)?,
            },
        })
    }
}

/// A bounds-checked reader over the bytes of one ERT.
struct Cursor<'bytes> {
    bytes: &'bytes [u8],
    position: usize,
    table: TableOrdinal,

    /// The variable count of the last encoded-text column read.
    num_variables: u64,
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
        let loaded = ColumnData::tile(
            encodings.iter().copied(),
            bytes,
            true,
            num_rows,
            TABLE,
            LoadedColumns::default(),
        )?;
        Ok(loaded.columns.into_boxed_slice())
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
    fn prefixes_tile_their_leading_columns_and_continue_after_them() -> anyhow::Result<()> {
        let encodings = [
            ColumnEncoding::Int64,
            ColumnEncoding::ClpString,
            ColumnEncoding::Boolean,
        ];
        let mut bytes = le_bytes(&[7, 8, logtype_word(5, 0), logtype_word(6, 1), 2, 10, 20]);
        bytes.extend([1, 0]);
        let tile_prefix = |size: usize, loaded: LoadedColumns| {
            ColumnData::tile(
                encodings.iter().copied(),
                &bytes[..size],
                false,
                2,
                TABLE,
                loaded,
            )
        };
        for (size, num_columns) in [(0, 0), (15, 0), (16, 1), (47, 1), (56, 2), (57, 2)] {
            let loaded = tile_prefix(size, LoadedColumns::default())?;
            assert_eq!(loaded.columns.len(), num_columns, "{size} bytes");
            let complete =
                ColumnData::tile(encodings.iter().copied(), &bytes, true, 2, TABLE, loaded)?;
            assert_eq!(
                complete.columns,
                tile(&encodings, &bytes, 2)?.into_vec(),
                "{size} bytes"
            );
        }

        let result = tile_prefix(58, LoadedColumns::default());
        assert!(
            matches!(result, Err(TableError::ColumnTiling(TABLE))),
            "every column fits a prefix shorter than the table: {result:?}"
        );
        let mut bad_offsets = bytes.clone();
        bad_offsets[16..24].copy_from_slice(&logtype_word(6, 3).to_le_bytes());
        let result = ColumnData::tile(
            encodings.iter().copied(),
            &bad_offsets[..56],
            false,
            2,
            TABLE,
            LoadedColumns::default(),
        );
        assert!(
            matches!(result, Err(TableError::VariableOutOfRange { .. })),
            "{result:?}"
        );
        Ok(())
    }

    #[test]
    fn leading_columns_are_measured_from_their_variable_counts() -> anyhow::Result<()> {
        let encodings = [
            ColumnEncoding::Int64,
            ColumnEncoding::ClpString,
            ColumnEncoding::Boolean,
        ];
        let mut bytes = le_bytes(&[7, 8, logtype_word(5, 0), logtype_word(6, 1), 2, 10, 20]);
        bytes.extend([1, 0]);
        let table_size = bytes.len() as u64;
        let measure = |prefix: &[u8], num_columns: usize| {
            ColumnData::measure(
                encodings.iter().copied().take(num_columns),
                prefix,
                2,
                table_size,
                TABLE,
            )
        };
        assert_eq!(measure(&[], 0)?, LeadingColumnsSize::Known(0));
        assert_eq!(measure(&[], 1)?, LeadingColumnsSize::Known(16));
        assert_eq!(measure(&bytes[..39], 2)?, LeadingColumnsSize::AtLeast(40));
        assert_eq!(measure(&bytes[..40], 2)?, LeadingColumnsSize::Known(56));
        assert_eq!(measure(&bytes, 3)?, LeadingColumnsSize::Known(table_size));

        let result =
            ColumnData::measure(encodings.iter().copied(), &bytes, 2, table_size - 1, TABLE);
        assert!(
            matches!(result, Err(TableError::ColumnTiling(TABLE))),
            "{result:?}"
        );
        let mut huge_count = bytes;
        huge_count[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
        let result = measure(&huge_count, 2);
        assert!(
            matches!(result, Err(TableError::ColumnTiling(TABLE))),
            "{result:?}"
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
