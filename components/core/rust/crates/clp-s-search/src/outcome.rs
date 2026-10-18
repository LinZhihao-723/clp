//! Outcome sets: the K3 values a leaf can take over a scope (a table, a column, an archive).

use clp_s_query::ColumnRef;
use clp_s_query::Leaf;
use clp_s_query::RecordValueSource;
use clp_s_query::ScalarValue;
use clp_s_query::TruthValue;

/// A subset of {True, False, Unknown}.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct OutcomeSet(u8);

impl OutcomeSet {
    pub const ALL: Self = Self(Self::TRUE.0 | Self::FALSE.0 | Self::UNKNOWN.0);
    pub const FALSE: Self = Self(2);
    pub const TRUE: Self = Self(1);
    pub const TRUE_OR_FALSE: Self = Self(Self::TRUE.0 | Self::FALSE.0);
    pub const UNKNOWN: Self = Self(4);

    /// # Returns
    ///
    /// The set holding only `value`.
    pub const fn of(value: TruthValue) -> Self {
        match value {
            TruthValue::True => Self::TRUE,
            TruthValue::False => Self::FALSE,
            TruthValue::Unknown => Self::UNKNOWN,
        }
    }

    /// # Returns
    ///
    /// Whether `value` is in the set.
    pub const fn contains(self, value: TruthValue) -> bool {
        0 != self.0 & Self::of(value).0
    }

    /// # Returns
    ///
    /// The union of both sets.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// # Returns
    ///
    /// The values an occurrence of the leaf selects: True for a positive occurrence, False for a
    /// negated one.
    pub const fn selected_value(is_negated: bool) -> TruthValue {
        if is_negated {
            TruthValue::False
        } else {
            TruthValue::True
        }
    }
}

/// A record source that reaches exactly one value whatever the column: evaluates a leaf on one
/// stored value, a constant node or a range bound.
pub struct SingleValue<'value>(pub ScalarValue<'value>);

impl SingleValue<'_> {
    /// # Returns
    ///
    /// The truth value of `leaf` on the single value.
    pub fn evaluate(&self, leaf: &Leaf) -> TruthValue {
        leaf.evaluate(self)
    }
}

impl RecordValueSource for SingleValue<'_> {
    fn visit_values<VisitorType: FnMut(ScalarValue<'_>) -> bool>(
        &self,
        _column: &ColumnRef,
        mut visit: VisitorType,
    ) {
        visit(self.0);
    }
}

/// Joins the outcome sets of the values one existential leaf reaches in the same record, as
/// independent sets: True if some value can be True; False if some value can be False and
/// every value can be non-True; Unknown if every value can be Unknown.
///
/// # Type Parameters
///
/// * `SetsType` - Iterates over the per-value sets.
///
/// # Returns
///
/// The joined set, or `None` if `sets` is empty (no value is reached).
#[cfg(test)]
pub fn existential_join<SetsType: IntoIterator<Item = OutcomeSet>>(
    sets: SetsType,
) -> Option<OutcomeSet> {
    let mut is_any = false;
    let mut can_be_true = false;
    let mut can_be_false = false;
    let mut can_all_be_non_true = true;
    let mut can_all_be_unknown = true;
    for set in sets {
        is_any = true;
        can_be_true |= set.contains(TruthValue::True);
        can_be_false |= set.contains(TruthValue::False);
        can_all_be_non_true &= set.contains(TruthValue::False) || set.contains(TruthValue::Unknown);
        can_all_be_unknown &= set.contains(TruthValue::Unknown);
    }
    if !is_any {
        return None;
    }
    let mut joined = OutcomeSet::default();
    if can_be_true {
        joined = joined.union(OutcomeSet::TRUE);
    }
    if can_be_false && can_all_be_non_true {
        joined = joined.union(OutcomeSet::FALSE);
    }
    if can_all_be_unknown {
        joined = joined.union(OutcomeSet::UNKNOWN);
    }
    Some(joined)
}

#[cfg(test)]
mod tests {
    use clp_s_query::TruthValue;

    use super::OutcomeSet;
    use super::existential_join;

    #[test]
    fn joins_follow_the_existential_rule() {
        let t = OutcomeSet::TRUE;
        let f = OutcomeSet::FALSE;
        let u = OutcomeSet::UNKNOWN;
        let tf = OutcomeSet::TRUE_OR_FALSE;
        for (sets, expected) in [
            (vec![t, f], Some(t)),
            (vec![tf, f], Some(tf)),
            (vec![u, f], Some(f)),
            (vec![u, u], Some(u)),
            (vec![tf, u], Some(tf)),
            (vec![OutcomeSet::ALL, u], Some(OutcomeSet::ALL)),
            (vec![tf, tf], Some(tf)),
            (vec![], None),
        ] {
            assert_eq!(existential_join(sets.iter().copied()), expected, "{sets:?}");
        }
    }

    #[test]
    fn membership_and_selection_values() {
        assert!(OutcomeSet::TRUE_OR_FALSE.contains(TruthValue::False));
        assert!(!OutcomeSet::TRUE_OR_FALSE.contains(TruthValue::Unknown));
        assert_eq!(OutcomeSet::selected_value(true), TruthValue::False);
        assert_eq!(OutcomeSet::selected_value(false), TruthValue::True);
    }
}
