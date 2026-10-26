//! Leaf kernels: the bound columns of one leaf, each with a probe specialized for its encoding,
//! evaluated over the active rows of a window into True and False masks.

use std::sync::Arc;

use clp_s_format::VariableId;
use clp_s_query::Leaf;
use clp_s_query::PredicateTest;
use clp_s_query::ScalarValue;
use clp_s_query::TruthValue;
use clp_s_table::ColumnBatch;
use clp_s_table::ColumnEncoding;
use clp_s_table::ColumnIndex;
use clp_s_table::EncodedTextBatch;
use clp_s_table::LeI64;
use clp_s_table::TableError;
use clp_s_table::TableLayout;
use clp_s_table::WindowColumns;

use crate::ColumnTest;
use crate::EncodedSubquery;
use crate::EncodedTextMatch;
use crate::EncodedTextMember;
use crate::EncodedVariableTest;
use crate::EvalError;
use crate::IdSet;
use crate::LeafBinding;
use crate::RangeOutcomes;
use crate::filter::array::ArrayProbe;
use crate::filter::array::WalkScratch;
use crate::filter::mask;
use crate::filter::mask::MaskPool;
use crate::filter::value;
use crate::filter::value::ElementTest;
use crate::filter::value::KeyDomain;
use crate::filter::value::KeyTest;
use crate::filter::value::float_key;
use crate::outcome::SingleValue;

/// The cost classes kernels are ordered by, cheapest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CostClass {
    Constant,
    Scalar,
    DictionaryId,
    RowRange,
    EncodedText,
    DecodedText,
    Array,
}

/// The kernel of one leaf on one table: the existential join of its bound columns.
#[derive(Debug)]
pub struct LeafKernel {
    /// Cheapest first.
    columns: Box<[ColumnKernel]>,

    is_false_without_columns: bool,
    cost: CostClass,
}

impl LeafKernel {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The kernel of `leaf` bound to `layout` by `binding` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`EvalError::PlanMismatch`] if a bound column is not in `layout`, or its test does not fit
    ///   the column's encoding.
    pub fn new(
        leaf: &Leaf,
        binding: &LeafBinding,
        layout: &TableLayout,
    ) -> Result<Self, EvalError> {
        let mut columns = binding
            .columns
            .iter()
            .map(|bound| {
                let spec = layout
                    .columns()
                    .get(bound.column.get() as usize)
                    .filter(|spec| spec.node_id == bound.node_id)
                    .ok_or(EvalError::PlanMismatch {
                        reason: "a bound column is not a column of its node in the table layout",
                    })?;
                Ok(ColumnKernel {
                    column: bound.column,
                    probe: Probe::new(leaf, &bound.test, spec.encoding)?,
                })
            })
            .collect::<Result<Vec<_>, EvalError>>()?;
        columns.sort_by_key(|column| column.probe.cost());
        let cost = columns
            .iter()
            .map(|column| column.probe.cost())
            .max()
            .unwrap_or(CostClass::Constant);
        Ok(Self {
            columns: columns.into_boxed_slice(),
            is_false_without_columns: binding.is_false_without_columns,
            cost,
        })
    }

    /// # Returns
    ///
    /// The cost class of the kernel: that of its most expensive column.
    pub const fn cost(&self) -> CostClass {
        self.cost
    }

    /// # Returns
    ///
    /// The columns the kernel reads.
    pub fn columns(&self) -> impl Iterator<Item = ColumnIndex> + '_ {
        self.columns.iter().map(|column| column.column)
    }

    /// Selects the active rows on which an occurrence of the leaf is True: the rows where the leaf
    /// is True for a positive occurrence, False for a negated one.
    ///
    /// Columns run cheapest first, each only on the active rows no earlier column made True.
    ///
    /// # Returns
    ///
    /// The selected rows (a mask taken from `masks`) on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`ColumnKernel::evaluate`]'s return values on failure.
    pub fn evaluate(
        &self,
        columns: &WindowColumns<'_>,
        active: &[u64],
        is_negated: bool,
        masks: &mut MaskPool,
        buffers: &mut KernelBuffers,
    ) -> Result<Vec<u64>, EvalError> {
        let is_false_needed = is_negated && !self.is_false_without_columns;
        let mut truthy = masks.zeroed();
        let mut falsy = masks.zeroed();
        let mut remaining = masks.copy_of(active);
        let mut column_truthy = masks.zeroed();
        let mut column_falsy = masks.zeroed();
        for column in &self.columns {
            if mask::is_empty(&remaining) {
                break;
            }
            column_truthy.fill(0);
            column_falsy.fill(0);
            column.evaluate(
                columns,
                &remaining,
                &mut column_truthy,
                is_false_needed.then_some(column_falsy.as_mut_slice()),
                buffers,
            )?;
            mask::or_assign(&mut truthy, &column_truthy);
            mask::or_assign(&mut falsy, &column_falsy);
            mask::and_not_assign(&mut remaining, &column_truthy);
        }
        masks.recycle(column_truthy);
        masks.recycle(column_falsy);
        masks.recycle(remaining);
        if !is_negated {
            masks.recycle(falsy);
            return Ok(truthy);
        }
        let mut selected = if self.is_false_without_columns {
            masks.recycle(falsy);
            masks.copy_of(active)
        } else {
            falsy
        };
        mask::and_not_assign(&mut selected, &truthy);
        masks.recycle(truthy);
        Ok(selected)
    }
}

/// Reusable buffers of kernels that decode values.
#[derive(Debug, Default)]
pub struct KernelBuffers {
    text: Vec<u8>,
    walk: WalkScratch,
    candidates: Vec<u64>,
}

/// One bound column of a leaf.
#[derive(Debug)]
struct ColumnKernel {
    column: ColumnIndex,
    probe: Probe,
}

impl ColumnKernel {
    /// Writes into `truthy` the `active` rows on which the column is True and, if asked, into
    /// `falsy` those on which it is False.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`WindowColumns::column`]'s return values on failure.
    /// * Forwards [`Probe::evaluate_rows`]'s return values on failure.
    fn evaluate(
        &self,
        columns: &WindowColumns<'_>,
        active: &[u64],
        truthy: &mut [u64],
        falsy: Option<&mut [u64]>,
        buffers: &mut KernelBuffers,
    ) -> Result<(), EvalError> {
        let batch = columns.column(self.column)?;
        let num_rows = batch.num_rows() as usize;
        match (&self.probe, batch) {
            (Probe::Key(test), ColumnBatch::Int64(values)) => {
                write_key_masks(test, active, num_rows, truthy, falsy, |row| {
                    values[row].get()
                });
            }
            (
                Probe::Key(test),
                ColumnBatch::DeltaInt64(values)
                | ColumnBatch::Timestamp {
                    epoch_ns: values, ..
                },
            ) => write_key_masks(test, active, num_rows, truthy, falsy, |row| values[row]),
            (
                Probe::Key(test),
                ColumnBatch::Float64(values) | ColumnBatch::FormattedFloat { values, .. },
            ) => write_key_masks(test, active, num_rows, truthy, falsy, |row| {
                float_key(values[row].get())
            }),
            (Probe::Boolean { on_true, on_false }, ColumnBatch::Boolean(values)) => {
                write_truth_masks(active, num_rows, truthy, falsy, |row| {
                    if 0 == values[row] {
                        *on_false
                    } else {
                        *on_true
                    }
                });
            }
            (Probe::VariableIds(ids), ColumnBatch::VariableString { ids: values, .. }) => {
                mask::write_word_masks(active, num_rows, truthy, falsy, |row| {
                    ids.contains(values[row].get())
                });
            }
            (probe, batch) => probe.evaluate_rows(batch, active, truthy, falsy, buffers)?,
        }
        Ok(())
    }
}

/// How one bound column is tested, specialized for its encoding.
#[derive(Debug)]
enum Probe {
    /// An ordered column (integers, plain and formatted floats, timestamps) against key ranges.
    Key(KeyTest),

    /// A dictionary-float column: each value's text is parsed and tested against float ranges.
    DictionaryFloat(KeyTest),

    Boolean {
        on_true: TruthValue,
        on_false: TruthValue,
    },

    VariableIds(Arc<IdSet>),

    EncodedText {
        members: Arc<[EncodedTextMember]>,
        is_case_sensitive: bool,
        cost: CostClass,

        /// The logtypes some member can match on, or `None` if a member decodes every value.
        logtypes: Option<LogtypeSet>,
    },

    Array(ArrayProbe),

    LogEventRanges(Arc<RangeOutcomes>),
}

impl Probe {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The probe of `leaf` through `test` on a column encoded as `encoding` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`EvalError::PlanMismatch`] if `test` does not fit `encoding`.
    fn new(leaf: &Leaf, test: &ColumnTest, encoding: ColumnEncoding) -> Result<Self, EvalError> {
        let key = |domain| Self::Key(KeyTest::of_leaf(leaf, domain));
        Ok(match (test, encoding) {
            (ColumnTest::Value, ColumnEncoding::Int64 | ColumnEncoding::DeltaInt64) => {
                key(KeyDomain::Integer)
            }
            (ColumnTest::Value, ColumnEncoding::Float64 | ColumnEncoding::FormattedFloat64) => {
                key(KeyDomain::Float)
            }
            (ColumnTest::Value, ColumnEncoding::Timestamp) => key(KeyDomain::Timestamp),
            (ColumnTest::Value, ColumnEncoding::DictionaryFloat) => {
                Self::DictionaryFloat(KeyTest::of_leaf(leaf, KeyDomain::Float))
            }
            (ColumnTest::Value, ColumnEncoding::Boolean) => Self::Boolean {
                on_true: SingleValue(ScalarValue::Boolean(true)).evaluate(leaf),
                on_false: SingleValue(ScalarValue::Boolean(false)).evaluate(leaf),
            },
            (ColumnTest::VariableIds(ids), ColumnEncoding::VariableString) => {
                Self::VariableIds(Arc::clone(ids))
            }
            (ColumnTest::EncodedText(members), ColumnEncoding::ClpString) => {
                let is_decoding = members.iter().any(|member| match &member.prefilter {
                    EncodedTextMatch::Never => false,
                    EncodedTextMatch::DecodeAll => true,
                    EncodedTextMatch::Subqueries(subqueries) => subqueries
                        .iter()
                        .any(|subquery| subquery.is_glob_check_required),
                });
                Self::EncodedText {
                    members: Arc::clone(members),
                    is_case_sensitive: is_case_sensitive(leaf),
                    cost: if is_decoding {
                        CostClass::DecodedText
                    } else {
                        CostClass::EncodedText
                    },
                    logtypes: LogtypeSet::of_members(members),
                }
            }
            (ColumnTest::Array(access), ColumnEncoding::UnstructuredArray) => {
                Self::Array(ArrayProbe::new(access, ElementTest::new(leaf)))
            }
            (
                ColumnTest::LogEventRanges(ranges),
                ColumnEncoding::Int64 | ColumnEncoding::DeltaInt64,
            ) => Self::LogEventRanges(Arc::clone(ranges)),
            _ => {
                return Err(EvalError::PlanMismatch {
                    reason: "a column test does not fit the column's encoding",
                });
            }
        })
    }

    /// # Returns
    ///
    /// The cost class of the probe.
    const fn cost(&self) -> CostClass {
        match self {
            Self::Key(_) | Self::Boolean { .. } => CostClass::Scalar,
            Self::EncodedText { cost, .. } => *cost,
            Self::VariableIds(_) | Self::DictionaryFloat(_) => CostClass::DictionaryId,
            Self::LogEventRanges(_) => CostClass::RowRange,
            Self::Array(_) => CostClass::Array,
        }
    }

    /// Evaluates the probes that work row by row, on the active rows only.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`EvalError::PlanMismatch`] if `batch` does not have the compiled encoding.
    /// * [`EvalError::Table`] if a dictionary id is out of range, or a value cannot be decoded.
    /// * [`EvalError::InvalidDictionaryFloat`] if a dictionary-float entry does not parse as a
    ///   float.
    /// * Forwards [`ArrayProbe::evaluate`]'s return values on failure.
    fn evaluate_rows(
        &self,
        batch: ColumnBatch<'_>,
        active: &[u64],
        truthy: &mut [u64],
        mut falsy: Option<&mut [u64]>,
        buffers: &mut KernelBuffers,
    ) -> Result<(), EvalError> {
        let mut write = |row: u32, value: TruthValue| {
            let (word, bit) = ((row / 64) as usize, 1_u64 << (row % 64));
            match value {
                TruthValue::True => truthy[word] |= bit,
                TruthValue::False => {
                    if let Some(falsy) = falsy.as_deref_mut() {
                        falsy[word] |= bit;
                    }
                }
                TruthValue::Unknown => {}
            }
        };
        match (self, batch) {
            (Self::DictionaryFloat(test), ColumnBatch::DictionaryFloat { ids, dictionary }) => {
                mask::try_for_each_row(active, |row| {
                    let id = ids[row as usize].get();
                    let entry = dictionary
                        .get(VariableId::new(id))
                        .map_err(TableError::from)?;
                    let value = std::str::from_utf8(entry)
                        .ok()
                        .and_then(|text| text.parse::<f64>().ok())
                        .ok_or(EvalError::InvalidDictionaryFloat { id })?;
                    write(row, test.outcome(float_key(value)));
                    Ok::<(), EvalError>(())
                })
            }
            (
                Self::EncodedText {
                    members,
                    is_case_sensitive,
                    logtypes,
                    ..
                },
                ColumnBatch::ClpString(encoded),
            ) => {
                let KernelBuffers {
                    text, candidates, ..
                } = buffers;
                let candidates = logtypes.as_ref().map_or(active, |logtypes| {
                    logtypes.select_candidates(&encoded, active, candidates, falsy.as_deref_mut())
                });
                mask::try_for_each_row(candidates, |row| {
                    let is_match =
                        encoded_text_matches(&encoded, row, members, *is_case_sensitive, text)?;
                    let (word, bit) = ((row / 64) as usize, 1_u64 << (row % 64));
                    if is_match {
                        truthy[word] |= bit;
                    } else if let Some(falsy) = falsy.as_deref_mut() {
                        falsy[word] |= bit;
                    }
                    Ok(())
                })
            }
            (Self::Array(probe), ColumnBatch::UnstructuredArray(encoded)) => {
                mask::try_for_each_row(active, |row| {
                    buffers.text.clear();
                    encoded.decode(row, &mut buffers.text)?;
                    write(row, probe.evaluate(&buffers.text, &mut buffers.walk)?);
                    Ok(())
                })
            }
            (Self::LogEventRanges(ranges), ColumnBatch::Int64(values)) => {
                mask::try_for_each_row(active, |row| {
                    write(row, range_outcome(ranges, values[row as usize].get()));
                    Ok(())
                })
            }
            (Self::LogEventRanges(ranges), ColumnBatch::DeltaInt64(values)) => {
                mask::try_for_each_row(active, |row| {
                    write(row, range_outcome(ranges, values[row as usize]));
                    Ok(())
                })
            }
            _ => Err(EvalError::PlanMismatch {
                reason: "a window column does not have the compiled encoding",
            }),
        }
    }
}

/// A set of logtype ids.
#[derive(Debug)]
enum LogtypeSet {
    Single(u64),

    /// Bit `id - base` of the words is set for every id.
    Dense {
        base: u64,
        words: Box<[u64]>,
    },
}

impl LogtypeSet {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The logtypes some subquery of `members` matches, or `None` if a member decodes every value.
    fn of_members(members: &[EncodedTextMember]) -> Option<Self> {
        let mut ids = Vec::new();
        for member in members {
            match &member.prefilter {
                EncodedTextMatch::Never => {}
                EncodedTextMatch::DecodeAll => return None,
                EncodedTextMatch::Subqueries(subqueries) => {
                    ids.extend(
                        subqueries
                            .iter()
                            .flat_map(|subquery| subquery.logtypes.to_vec()),
                    );
                }
            }
        }
        ids.sort_unstable();
        ids.dedup();
        if let [id] = ids.as_slice() {
            return Some(Self::Single(*id));
        }
        let base = ids.first().copied().unwrap_or_default();
        let num_words = ids.last().map_or(0, |last| (last - base) / 64 + 1);
        let mut words = vec![0_u64; usize::try_from(num_words).ok()?];
        for id in ids {
            let offset = id - base;
            words[usize::try_from(offset / 64).ok()?] |= 1 << (offset % 64);
        }
        Some(Self::Dense {
            base,
            words: words.into_boxed_slice(),
        })
    }

    /// Selects the `active` rows of `batch` whose logtype is in the set into `candidates` and, if
    /// given, writes the others into `falsy`.
    ///
    /// # Returns
    ///
    /// The selected rows.
    fn select_candidates<'candidates>(
        &self,
        batch: &EncodedTextBatch<'_>,
        active: &[u64],
        candidates: &'candidates mut Vec<u64>,
        mut falsy: Option<&mut [u64]>,
    ) -> &'candidates [u64] {
        candidates.clear();
        candidates.resize(active.len(), 0);
        let num_rows = batch.num_rows() as usize;
        for (word_index, active_word) in active.iter().copied().enumerate() {
            if 0 == active_word {
                continue;
            }
            let base = word_index * 64;
            let ids = batch.logtype_ids(base..num_rows.min(base + 64));
            let bits = match self {
                Self::Single(single) => ids.zip(0_u32..).fold(0_u64, |bits, (id, offset)| {
                    bits | (u64::from(*single == id) << offset)
                }),
                Self::Dense { base, words } => {
                    ids.zip(0_u32..).fold(0_u64, |bits, (id, row_offset)| {
                        let is_member = id.checked_sub(*base).is_some_and(|offset| {
                            usize::try_from(offset / 64)
                                .ok()
                                .and_then(|index| words.get(index))
                                .is_some_and(|word| 0 != word & (1 << (offset % 64)))
                        });
                        bits | (u64::from(is_member) << row_offset)
                    })
                }
            };
            candidates[word_index] = bits & active_word;
            if let Some(falsy) = falsy.as_deref_mut() {
                falsy[word_index] = !bits & active_word;
            }
        }
        candidates
    }
}

/// Writes the masks of an ordered column whose row `row` has the key `key_of(row)`.
///
/// # Type Parameters
///
/// * `KeyType` - Returns the key of one window row.
fn write_key_masks<KeyType: Fn(usize) -> i64>(
    test: &KeyTest,
    active: &[u64],
    num_rows: usize,
    truthy: &mut [u64],
    falsy: Option<&mut [u64]>,
    key_of: KeyType,
) {
    match test {
        KeyTest::Constant(value) => write_truth_masks(active, num_rows, truthy, falsy, |_| *value),
        KeyTest::Ranges(ranges) => match &**ranges {
            [range] => {
                let (first, last) = (range.first, range.last);
                mask::write_word_masks(active, num_rows, truthy, falsy, |row| {
                    let key = key_of(row);
                    first <= key && key <= last
                });
            }
            ranges => mask::write_word_masks(active, num_rows, truthy, falsy, |row| {
                value::contains(ranges, key_of(row))
            }),
        },
    }
}

/// # Returns
///
/// The outcome of a range-index leaf on the row with `log_event_idx` (Unknown for a negative
/// index, which no range holds).
fn range_outcome(ranges: &RangeOutcomes, log_event_idx: i64) -> TruthValue {
    u64::try_from(log_event_idx)
        .ok()
        .and_then(|log_event_idx| ranges.outcome(log_event_idx))
        .map_or(TruthValue::Unknown, value::truth)
}

/// Writes the masks of a column whose row `row` has the truth value `value_of(row)`.
///
/// # Type Parameters
///
/// * `ValueType` - Returns the truth value of one window row.
fn write_truth_masks<ValueType: Fn(usize) -> TruthValue>(
    active: &[u64],
    num_rows: usize,
    truthy: &mut [u64],
    falsy: Option<&mut [u64]>,
    value_of: ValueType,
) {
    mask::write_word_masks(active, num_rows, truthy, None, |row| {
        TruthValue::True == value_of(row)
    });
    if let Some(falsy) = falsy {
        mask::write_word_masks(active, num_rows, falsy, None, |row| {
            TruthValue::False == value_of(row)
        });
    }
}

/// Matches one `ClpString` value against the members of a leaf with their prefilters: a member
/// whose prefilter rejects the value's logtype and variables is skipped, a matching subquery
/// without a glob check matches, and anything else decodes the value (once) and matches the glob.
///
/// # Returns
///
/// Whether some member matches on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`EncodedTextBatch::variables`]'s return values on failure.
/// * Forwards [`EncodedTextBatch::decode`]'s return values on failure.
fn encoded_text_matches(
    batch: &EncodedTextBatch<'_>,
    row: u32,
    members: &[EncodedTextMember],
    is_case_sensitive: bool,
    text: &mut Vec<u8>,
) -> Result<bool, TableError> {
    let logtype = batch.logtype_id(row).get();
    let mut variables: Option<&[LeI64]> = None;
    let mut is_decoded = false;
    for member in members {
        let is_glob_check_required = match &member.prefilter {
            EncodedTextMatch::Never => continue,
            EncodedTextMatch::DecodeAll => true,
            EncodedTextMatch::Subqueries(subqueries) => {
                match subqueries_match(subqueries, logtype, batch, row, &mut variables)? {
                    None => continue,
                    Some(is_glob_check_required) => is_glob_check_required,
                }
            }
        };
        if !is_glob_check_required {
            return Ok(true);
        }
        if !is_decoded {
            text.clear();
            batch.decode(row, text)?;
            is_decoded = true;
        }
        if member.pattern.matches(text, is_case_sensitive) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Matches a value's logtype and variables against the subqueries of one prefilter, reading the
/// variables (into `variables`) only once a logtype matches.
///
/// # Returns
///
/// `None` if no subquery matches, else whether every matching subquery requires a glob check, on
/// success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`EncodedTextBatch::variables`]'s return values on failure.
fn subqueries_match<'batch>(
    subqueries: &[EncodedSubquery],
    logtype: u64,
    batch: &EncodedTextBatch<'batch>,
    row: u32,
    variables: &mut Option<&'batch [LeI64]>,
) -> Result<Option<bool>, TableError> {
    let mut result = None;
    for subquery in subqueries {
        if !subquery.logtypes.contains(logtype) {
            continue;
        }
        if !subquery.variables.is_empty() {
            let values = match *variables {
                Some(values) => values,
                None => *variables.insert(batch.variables(row)?),
            };
            if !variables_match_in_order(&subquery.variables, values) {
                continue;
            }
        }
        if !subquery.is_glob_check_required {
            return Ok(Some(false));
        }
        result = Some(true);
    }
    Ok(result)
}

/// # Returns
///
/// Whether `variables` contains, in order and not necessarily contiguously, a value passing each
/// test (C++ `SubQuery::matches_vars`).
fn variables_match_in_order(tests: &[EncodedVariableTest], variables: &[LeI64]) -> bool {
    let mut tests = tests.iter().peekable();
    for variable in variables {
        if let Some(test) = tests.peek()
            && test.matches(variable.get())
        {
            tests.next();
        }
    }
    tests.peek().is_none()
}

/// # Returns
///
/// Whether `leaf`'s comparison is case-sensitive (`Exists` has no case, so it counts as
/// case-sensitive).
const fn is_case_sensitive(leaf: &Leaf) -> bool {
    match &leaf.test {
        PredicateTest::Compare {
            is_case_sensitive, ..
        }
        | PredicateTest::InSet {
            is_case_sensitive, ..
        } => *is_case_sensitive,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Range;
    use std::sync::Arc;

    use clp_s_format::ReaderLimits;
    use clp_s_format::VariableDictionary;
    use clp_s_query::Query;
    use clp_s_query::QueryOptions;
    use clp_s_query::TruthValue;
    use clp_s_table::ColumnBatch;
    use clp_s_table::LeI64;
    use clp_s_table::LeU64;

    use super::CostClass;
    use super::KernelBuffers;
    use super::Probe;
    use super::variables_match_in_order;
    use super::write_key_masks;
    use crate::EncodedVariableTest;
    use crate::IdSet;
    use crate::RangeOutcomes;
    use crate::filter::mask;
    use crate::filter::value::KeyDomain;
    use crate::filter::value::KeyRange;
    use crate::filter::value::KeyTest;

    /// # Returns
    ///
    /// The rows set in `mask`.
    fn rows(mask: &[u64]) -> Vec<u32> {
        let mut rows = Vec::new();
        mask::try_for_each_row::<(), _>(mask, |row| {
            rows.push(row);
            Ok(())
        })
        .expect("collecting rows never fails");
        rows
    }

    /// # Returns
    ///
    /// The mask of `rows` over `num_words` words.
    fn mask_of(rows: &[u32], num_words: usize) -> Vec<u64> {
        let mut mask = vec![0_u64; num_words];
        for row in rows {
            mask[(row / 64) as usize] |= 1 << (row % 64);
        }
        mask
    }

    #[test]
    fn key_masks_follow_the_key_test() {
        let keys: Vec<i64> = (0..100).map(|key| key - 50).collect();
        let active_rows: Vec<u32> = (0..100).filter(|row| 0 != row % 7).collect();
        let active = mask_of(&active_rows, 2);
        let single = KeyTest::Ranges(Box::new([KeyRange {
            first: -3,
            last: 10,
        }]));
        let several = KeyTest::Ranges(Box::new([
            KeyRange {
                first: -50,
                last: -49,
            },
            KeyRange { first: 0, last: 0 },
            KeyRange {
                first: 40,
                last: 60,
            },
        ]));
        for test in [
            single,
            several,
            KeyTest::Ranges(Box::new([])),
            KeyTest::Constant(TruthValue::True),
            KeyTest::Constant(TruthValue::False),
            KeyTest::Constant(TruthValue::Unknown),
        ] {
            let mut truthy = vec![0_u64; 2];
            let mut falsy = vec![0_u64; 2];
            write_key_masks(
                &test,
                &active,
                keys.len(),
                &mut truthy,
                Some(&mut falsy),
                |row| keys[row],
            );
            let expected = |value: TruthValue| -> Vec<u32> {
                active_rows
                    .iter()
                    .copied()
                    .filter(|row| value == test.outcome(keys[*row as usize]))
                    .collect()
            };
            assert_eq!(rows(&truthy), expected(TruthValue::True), "{test:?}");
            assert_eq!(rows(&falsy), expected(TruthValue::False), "{test:?}");
        }
    }

    #[test]
    fn log_event_ranges_select_by_the_containing_range() -> anyhow::Result<()> {
        let outcomes = RangeOutcomes {
            true_ranges: Box::new([Range { start: 2, end: 5 }]),
            false_ranges: Box::new([Range { start: 7, end: 9 }]),
            is_false_outside_ranges: false,
        };
        let probe = Probe::LogEventRanges(Arc::new(outcomes));
        let values: Vec<i64> = vec![0, 2, 4, 5, 7, 8, 9, -1, 3];
        let encoded: Vec<LeI64> = values.iter().copied().map(LeI64::new).collect();
        let active = mask_of(&[0, 1, 2, 3, 4, 5, 6, 7], 1);
        for batch in [
            ColumnBatch::Int64(&encoded),
            ColumnBatch::DeltaInt64(&values),
        ] {
            let mut truthy = vec![0_u64];
            let mut falsy = vec![0_u64];
            probe.evaluate_rows(
                batch,
                &active,
                &mut truthy,
                Some(&mut falsy),
                &mut KernelBuffers::default(),
            )?;
            assert_eq!(rows(&truthy), vec![1, 2], "{batch:?}");
            assert_eq!(rows(&falsy), vec![4, 5], "{batch:?}");
        }
        let mismatch = probe.evaluate_rows(
            ColumnBatch::Boolean(&[1]),
            &active,
            &mut [0],
            None,
            &mut KernelBuffers::default(),
        );
        assert!(
            matches!(mismatch, Err(crate::EvalError::PlanMismatch { .. })),
            "{mismatch:?}"
        );
        Ok(())
    }

    #[test]
    fn dictionary_float_entries_that_are_not_floats_are_corrupt() -> anyhow::Result<()> {
        let entries: [&[u8]; 3] = [b"1.50", b"1.5x", b"\xff"];
        let mut records = Vec::new();
        for entry in entries {
            records.extend_from_slice(&(entry.len() as u64).to_le_bytes());
            records.extend_from_slice(entry);
        }
        let mut section = (entries.len() as u64).to_le_bytes().to_vec();
        section.extend(zstd::encode_all(records.as_slice(), 0)?);
        let dictionary = VariableDictionary::load(&section, &ReaderLimits::default())?;
        let query = Query::from_kql("f > 1", &QueryOptions::default())?;
        let probe = Probe::DictionaryFloat(KeyTest::of_leaf(&query.leaves()[0], KeyDomain::Float));
        let ids: Vec<LeU64> = (0..3).map(LeU64::new).collect();
        let batch = ColumnBatch::DictionaryFloat {
            ids: &ids,
            dictionary: &dictionary,
        };
        let mut truthy = vec![0_u64];
        probe.evaluate_rows(
            batch,
            &mask_of(&[0], 1),
            &mut truthy,
            None,
            &mut KernelBuffers::default(),
        )?;
        assert_eq!(rows(&truthy), vec![0]);
        for id in [1_u32, 2] {
            let result = probe.evaluate_rows(
                batch,
                &mask_of(&[id], 1),
                &mut [0],
                None,
                &mut KernelBuffers::default(),
            );
            assert!(
                matches!(
                    result,
                    Err(crate::EvalError::InvalidDictionaryFloat { id: actual })
                        if u64::from(id) == actual
                ),
                "entry {id}: {result:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn variables_match_in_order_but_not_contiguously() {
        let variables: Vec<LeI64> = [5_i64, 7, 9, 7].into_iter().map(LeI64::new).collect();
        let exact = EncodedVariableTest::Exact;
        let ids = |ids: Vec<u64>| {
            EncodedVariableTest::DictionaryIds(IdSet::from_ids(ids).expect("non-empty ids"))
        };
        for (tests, expected) in [
            (vec![], true),
            (vec![exact(7)], true),
            (vec![exact(5), exact(9)], true),
            (vec![exact(9), exact(5)], false),
            (vec![exact(7), exact(7)], true),
            (vec![exact(7), exact(7), exact(7)], false),
            (vec![ids(vec![9, 100]), exact(7)], true),
            (vec![ids(vec![100])], false),
        ] {
            assert_eq!(
                variables_match_in_order(&tests, &variables),
                expected,
                "{tests:?}"
            );
        }
    }

    #[test]
    fn cost_classes_order_cheap_kernels_first() {
        let mut classes = vec![
            CostClass::Array,
            CostClass::DecodedText,
            CostClass::Scalar,
            CostClass::RowRange,
            CostClass::EncodedText,
            CostClass::DictionaryId,
            CostClass::Constant,
        ];
        classes.sort();
        assert_eq!(
            classes,
            [
                CostClass::Constant,
                CostClass::Scalar,
                CostClass::DictionaryId,
                CostClass::RowRange,
                CostClass::EncodedText,
                CostClass::DecodedText,
                CostClass::Array,
            ]
        );
    }
}
