//! Archive-scope facts from the metadata packets: timestamp-range outcomes of leaves and
//! range-index outcomes, used by Stage A and refined per table by Stage B.

use std::ops::Range;

use clp_s_format::MetadataFields;
use clp_s_format::MetadataValue;
use clp_s_format::RangeIndex;
use clp_s_format::TimestampIndexEntry;
use clp_s_format::TimestampRange;
use clp_s_query::ColumnRef;
use clp_s_query::CompareOperator;
use clp_s_query::JsonRecord;
use clp_s_query::Leaf;
use clp_s_query::PredicateTest;
use clp_s_query::QueryValue;
use clp_s_query::ScalarValue;
use clp_s_query::TruthValue;

use crate::RangeOutcomes;
use crate::outcome::OutcomeSet;
use crate::outcome::SingleValue;

/// Conservative nanosecond bounds of an indexed timestamp column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimestampBounds {
    pub min_epoch_ns: i64,
    pub max_epoch_ns: i64,
}

impl TimestampBounds {
    /// Factory function.
    ///
    /// The index stores floor and ceiling milliseconds, so every stored nanosecond value lies in
    /// the returned bounds (saturated at the `i64` range).
    ///
    /// # Returns
    ///
    /// The bounds of `entry`, or `None` if its range is unknown.
    pub const fn of_entry(entry: &TimestampIndexEntry) -> Option<Self> {
        const NANOSECONDS_PER_MILLISECOND: i64 = 1_000_000;
        match entry.range {
            Some(TimestampRange::EpochMillis { begin, end }) => Some(Self {
                min_epoch_ns: begin.saturating_mul(NANOSECONDS_PER_MILLISECOND),
                max_epoch_ns: end.saturating_mul(NANOSECONDS_PER_MILLISECOND),
            }),
            _ => None,
        }
    }

    /// Evaluates a leaf over every timestamp in the bounds. Comparisons are monotone in the
    /// value, so the bounds decide them: a leaf is {True} when it holds on the whole range,
    /// {False} when on none of it, and {Unknown} when its literal is not comparable with
    /// timestamps.
    ///
    /// # Returns
    ///
    /// The outcomes `leaf` can take on a timestamp value in the bounds.
    pub fn outcomes(self, leaf: &Leaf) -> OutcomeSet {
        match &leaf.test {
            PredicateTest::Exists => OutcomeSet::TRUE,
            PredicateTest::Compare {
                operator: CompareOperator::Equal,
                value,
                is_case_sensitive,
            } => self.equality_outcomes(value, *is_case_sensitive),
            PredicateTest::Compare { .. } => {
                let at_min = Self::evaluate_at(leaf, self.min_epoch_ns);
                let at_max = Self::evaluate_at(leaf, self.max_epoch_ns);
                if at_min == at_max {
                    OutcomeSet::of(at_min)
                } else {
                    OutcomeSet::TRUE_OR_FALSE
                }
            }
            PredicateTest::InSet {
                values,
                is_case_sensitive,
            } => {
                let mut is_any_comparable = false;
                let mut can_be_true = false;
                let mut can_all_be_false = true;
                for value in values {
                    let outcomes = self.equality_outcomes(value, *is_case_sensitive);
                    if OutcomeSet::UNKNOWN == outcomes {
                        continue;
                    }
                    is_any_comparable = true;
                    if OutcomeSet::TRUE == outcomes {
                        return OutcomeSet::TRUE;
                    }
                    can_be_true |= outcomes.contains(TruthValue::True);
                    can_all_be_false &= outcomes.contains(TruthValue::False);
                }
                if !is_any_comparable {
                    OutcomeSet::UNKNOWN
                } else if can_be_true {
                    OutcomeSet::TRUE_OR_FALSE
                } else if can_all_be_false {
                    OutcomeSet::FALSE
                } else {
                    OutcomeSet::TRUE_OR_FALSE
                }
            }
            _ => OutcomeSet::ALL,
        }
    }

    /// # Returns
    ///
    /// The outcomes of `x = value` over the timestamps `x` in the bounds.
    fn equality_outcomes(self, value: &QueryValue, is_case_sensitive: bool) -> OutcomeSet {
        let comparison = |operator: CompareOperator| Leaf {
            column: ColumnRef::AuthoritativeTimestamp,
            test: PredicateTest::Compare {
                operator,
                value: value.clone(),
                is_case_sensitive,
            },
            search_types: value.comparable_search_types(operator),
        };
        let at_min = Self::evaluate_at(&comparison(CompareOperator::Equal), self.min_epoch_ns);
        if TruthValue::Unknown == at_min {
            return OutcomeSet::UNKNOWN;
        }
        let is_below_range = TruthValue::True
            == Self::evaluate_at(&comparison(CompareOperator::Less), self.max_epoch_ns);
        let is_above_range = TruthValue::True
            == Self::evaluate_at(&comparison(CompareOperator::Greater), self.min_epoch_ns);
        if is_below_range || is_above_range {
            OutcomeSet::FALSE
        } else if self.min_epoch_ns == self.max_epoch_ns && TruthValue::True == at_min {
            OutcomeSet::TRUE
        } else {
            OutcomeSet::TRUE_OR_FALSE
        }
    }

    /// # Returns
    ///
    /// The truth value of `leaf` on the timestamp `epoch_ns`.
    fn evaluate_at(leaf: &Leaf, epoch_ns: i64) -> TruthValue {
        SingleValue(ScalarValue::Timestamp { epoch_ns }).evaluate(leaf)
    }
}

/// # Returns
///
/// The archive-scope outcomes of a leaf on [`ColumnRef::AuthoritativeTimestamp`]: Unknown alone
/// without an authoritative timestamp column, else the outcomes over its range plus Unknown (a
/// record may lack the timestamp).
pub fn authoritative_timestamp_outcomes(
    leaf: &Leaf,
    authoritative: Option<&TimestampIndexEntry>,
) -> OutcomeSet {
    let Some(entry) = authoritative else {
        return OutcomeSet::UNKNOWN;
    };
    TimestampBounds::of_entry(entry).map_or(OutcomeSet::ALL, |bounds| {
        bounds.outcomes(leaf).union(OutcomeSet::UNKNOWN)
    })
}

/// Evaluates a range-index leaf on every range of the range index, as the reference evaluator
/// evaluates it on the range's fields.
///
/// # Returns
///
/// The per-range outcomes, and the archive-scope outcome set (which includes the outcome of a
/// record outside every range).
pub fn range_index_outcomes(
    leaf: &Leaf,
    range_index: Option<&RangeIndex>,
) -> (RangeOutcomes, OutcomeSet) {
    let mut true_ranges: Vec<Range<u64>> = Vec::new();
    let mut false_ranges: Vec<Range<u64>> = Vec::new();
    let empty_record = serde_json::Value::Object(serde_json::Map::new());
    for entry in range_index.map_or(&[][..], RangeIndex::entries) {
        if entry.log_event_range.is_empty() {
            continue;
        }
        let fields = fields_to_json(&entry.fields);
        let source = JsonRecord::new(&empty_record, None).with_range_index_fields(&fields);
        match leaf.evaluate(&source) {
            TruthValue::True => true_ranges.push(entry.log_event_range.clone()),
            TruthValue::False => false_ranges.push(entry.log_event_range.clone()),
            TruthValue::Unknown => {}
        }
    }
    let is_false_outside_ranges =
        TruthValue::False == leaf.evaluate(&JsonRecord::new(&empty_record, None));
    let mut outcomes = OutcomeSet::of(if is_false_outside_ranges {
        TruthValue::False
    } else {
        TruthValue::Unknown
    });
    if !true_ranges.is_empty() {
        outcomes = outcomes.union(OutcomeSet::TRUE);
    }
    if !false_ranges.is_empty() {
        outcomes = outcomes.union(OutcomeSet::FALSE);
    }
    (
        RangeOutcomes {
            true_ranges: merge_ranges(true_ranges),
            false_ranges: merge_ranges(false_ranges),
            is_false_outside_ranges,
        },
        outcomes,
    )
}

/// # Returns
///
/// `ranges` sorted, with overlapping and adjacent ranges merged.
fn merge_ranges(mut ranges: Vec<Range<u64>>) -> Box<[Range<u64>]> {
    ranges.sort_by_key(|range| range.start);
    let mut merged: Vec<Range<u64>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged.into_boxed_slice()
}

/// Converts range-index fields into JSON for the reference evaluator. Binary values have no JSON
/// form and C++ skips them, so they are dropped; nested values are decoded from msgpack and
/// dropped if they have no JSON form.
///
/// # Returns
///
/// The fields as a JSON object.
fn fields_to_json(fields: &MetadataFields) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    for (key, value) in fields.to_vec() {
        let json = match value {
            MetadataValue::Null => Some(serde_json::Value::Null),
            MetadataValue::Boolean(value) => Some(serde_json::Value::Bool(value)),
            MetadataValue::Integer(value) => Some(value.into()),
            MetadataValue::UnsignedInteger(value) => Some(value.into()),
            MetadataValue::Float(value) => {
                serde_json::Number::from_f64(value).map(serde_json::Value::Number)
            }
            MetadataValue::String(text) => Some(serde_json::Value::String(text.to_owned())),
            MetadataValue::Nested(encoded) => rmp_serde::from_slice(encoded).ok(),
            MetadataValue::Binary(_) => None,
        };
        if let Some(json) = json {
            object.insert(key.to_owned(), json);
        }
    }
    serde_json::Value::Object(object)
}

#[cfg(test)]
mod tests {
    use clp_s_query::ColumnRef;
    use clp_s_query::Query;
    use clp_s_query::QueryOptions;

    use super::TimestampBounds;
    use crate::outcome::OutcomeSet;

    const BOUNDS: TimestampBounds = TimestampBounds {
        min_epoch_ns: 1_700_000_000_000_000_000,
        max_epoch_ns: 1_700_000_010_000_000_000,
    };

    /// # Returns
    ///
    /// The outcomes of the single leaf of `kql`, bound to the authoritative timestamp, over
    /// `bounds`.
    fn outcomes(kql: &str, bounds: TimestampBounds) -> OutcomeSet {
        let query = Query::from_kql(kql, &QueryOptions::default()).expect("the query should parse");
        let [leaf] = query.leaves() else {
            panic!("{kql} should have one leaf, not {:?}", query.leaves());
        };
        let mut leaf = leaf.clone();
        leaf.column = ColumnRef::AuthoritativeTimestamp;
        bounds.outcomes(&leaf)
    }

    #[test]
    fn range_comparisons_are_decided_by_the_bounds() {
        for (kql, expected) in [
            ("t > 1699999999999", OutcomeSet::TRUE),
            ("t >= 1700000000000", OutcomeSet::TRUE),
            ("t > 1700000000000", OutcomeSet::TRUE_OR_FALSE),
            ("t > 1700000010000", OutcomeSet::FALSE),
            ("t <= 1700000010000", OutcomeSet::TRUE),
            ("t < 1700000000000", OutcomeSet::FALSE),
            ("t < 1700000005000", OutcomeSet::TRUE_OR_FALSE),
            ("t: 1700000020000", OutcomeSet::FALSE),
            ("t: 1690000000000", OutcomeSet::FALSE),
            ("t: 1700000005000", OutcomeSet::TRUE_OR_FALSE),
            ("t: 1700000000000", OutcomeSet::TRUE_OR_FALSE),
            ("t: 1700000010000", OutcomeSet::TRUE_OR_FALSE),
            ("t: (1600000000000 1800000000000)", OutcomeSet::FALSE),
            (
                "t: (1600000000000 1700000005000)",
                OutcomeSet::TRUE_OR_FALSE,
            ),
            ("t: foo", OutcomeSet::UNKNOWN),
            ("t: true", OutcomeSet::UNKNOWN),
            ("t: *", OutcomeSet::TRUE),
            (r#"t > timestamp("1700000020", "\E")"#, OutcomeSet::FALSE),
            (r#"t < timestamp("1700000020", "\E")"#, OutcomeSet::TRUE),
        ] {
            assert_eq!(outcomes(kql, BOUNDS), expected, "{kql}");
        }
    }

    #[test]
    fn a_single_instant_makes_equality_true() {
        let instant = TimestampBounds {
            min_epoch_ns: 1_700_000_000_000_000_000,
            max_epoch_ns: 1_700_000_000_000_000_000,
        };
        assert_eq!(outcomes("t: 1700000000000", instant), OutcomeSet::TRUE);
        assert_eq!(outcomes("t: 1700000000001", instant), OutcomeSet::FALSE);
    }
}
