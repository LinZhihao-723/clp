//! Normalized queries.

use crate::ColumnRef;
use crate::CompareOperator;
use crate::LogicalExpr;
use crate::PredicateTest;
use crate::QueryError;
use crate::QueryValue;
use crate::RecordValueSource;
use crate::SearchTypeSet;
use crate::TruthValue;
use crate::compare::compare;
use crate::compare::equals_any;
use crate::normalize::LeafTable;
use crate::normalize::build_connective;
use crate::normalize::normalize;

/// Bounds on hostile query input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryLimits {
    /// Maximum nesting depth (default 1,024).
    pub max_depth: u32,

    /// Maximum number of distinct leaves (default 65,536).
    pub max_leaves: u32,
}

impl Default for QueryLimits {
    fn default() -> Self {
        Self {
            max_depth: 1024,
            max_leaves: 65_536,
        }
    }
}

/// Parse-time options; case sensitivity becomes a property of each predicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryOptions {
    pub is_case_sensitive: bool,
    pub limits: QueryLimits,
}

impl QueryOptions {
    /// # Returns
    ///
    /// These options with case sensitivity set to `is_case_sensitive`.
    #[must_use]
    pub const fn with_case_sensitivity(self, is_case_sensitive: bool) -> Self {
        Self {
            is_case_sensitive,
            ..self
        }
    }
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            is_case_sensitive: true,
            limits: QueryLimits::default(),
        }
    }
}

/// Inclusive bounds on the authoritative timestamp, in epoch nanoseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimeBounds {
    pub begin_epoch_ns: Option<i64>,
    pub end_epoch_ns: Option<i64>,
}

impl TimeBounds {
    /// Factory function.
    ///
    /// Maps C++ `--tge`/`--tle` milliseconds to nanoseconds, saturating instead of overflowing
    /// (DV-42).
    ///
    /// # Returns
    ///
    /// The bounds.
    #[must_use]
    pub const fn from_epoch_millis(begin_epoch_ms: Option<i64>, end_epoch_ms: Option<i64>) -> Self {
        const NANOSECONDS_PER_MILLISECOND: i64 = 1_000_000;
        Self {
            begin_epoch_ns: match begin_epoch_ms {
                Some(begin) => Some(begin.saturating_mul(NANOSECONDS_PER_MILLISECOND)),
                None => None,
            },
            end_epoch_ns: match end_epoch_ms {
                Some(end) => Some(end.saturating_mul(NANOSECONDS_PER_MILLISECOND)),
                None => None,
            },
        }
    }

    /// # Returns
    ///
    /// Whether neither bound is set.
    #[must_use]
    pub const fn is_unbounded(&self) -> bool {
        self.begin_epoch_ns.is_none() && self.end_epoch_ns.is_none()
    }
}

/// An archive-independent query in negation normal form over a table of distinct leaves.
#[derive(Clone, Debug)]
pub struct Query {
    leaves: Box<[Leaf]>,
    root: NormalExpr,
}

impl Query {
    /// Factory function.
    ///
    /// Parses KQL and normalizes it: NNF, K3 constant folding, flattening, hash-consing of
    /// leaves, `*`/`*:*` to [`NormalExpr::MatchAll`] and `NOT *` to [`NormalExpr::MatchNone`],
    /// and ORs of equalities on one column folded into [`PredicateTest::InSet`].
    ///
    /// # Returns
    ///
    /// The normalized query on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`LogicalExpr::parse_kql`]'s return values on failure.
    /// * Forwards [`Self::from_expr`]'s return values on failure.
    pub fn from_kql(text: &str, options: &QueryOptions) -> Result<Self, QueryError> {
        let expr = LogicalExpr::parse_kql(text, options)?;
        Self::from_expr(&expr, options)
    }

    /// Factory function.
    ///
    /// Normalizes a programmatic expression, after checking its depth iteratively.
    ///
    /// A subexpression that is Unknown for every record (a comparison whose literal no value can
    /// be compared with, such as `a > "foo"`) folds to [`NormalExpr::MatchNone`]: in negation
    /// normal form that never changes which records match.
    ///
    /// # Returns
    ///
    /// The normalized query on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`QueryError::LimitExceeded`] if the depth or the number of leaves exceeds
    ///   `options.limits`.
    pub fn from_expr(expr: &LogicalExpr, options: &QueryOptions) -> Result<Self, QueryError> {
        let (leaves, root) = normalize(expr, options.limits)?;
        Ok(Self {
            leaves: leaves.into_boxed_slice(),
            root,
        })
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The query that matches every record.
    #[must_use]
    pub fn match_all() -> Self {
        Self {
            leaves: Box::new([]),
            root: NormalExpr::MatchAll,
        }
    }

    /// # Returns
    ///
    /// The conjunction of this query and `bounds` as a leaf on
    /// [`ColumnRef::AuthoritativeTimestamp`] (unchanged if `bounds` is unbounded).
    ///
    /// Each set bound is one inclusive comparison leaf with a [`crate::QueryValue::Timestamp`]
    /// literal; a record without an authoritative timestamp makes it Unknown. Bounds with
    /// `begin > end` fold to [`NormalExpr::MatchNone`].
    #[must_use]
    pub fn with_time_bounds(&self, bounds: TimeBounds) -> Self {
        if bounds.is_unbounded() || NormalExpr::MatchNone == self.root {
            return self.clone();
        }
        if let (Some(begin), Some(end)) = (bounds.begin_epoch_ns, bounds.end_epoch_ns)
            && begin > end
        {
            return Self {
                leaves: self.leaves.clone(),
                root: NormalExpr::MatchNone,
            };
        }

        let mut table = LeafTable::from_leaves(self.leaves.to_vec());
        let mut bound_leaves = Vec::with_capacity(2);
        for (operator, epoch_ns) in [
            (CompareOperator::GreaterOrEqual, bounds.begin_epoch_ns),
            (CompareOperator::LessOrEqual, bounds.end_epoch_ns),
        ] {
            let Some(epoch_ns) = epoch_ns else {
                continue;
            };
            let test = PredicateTest::Compare {
                operator,
                value: QueryValue::Timestamp { epoch_ns },
                is_case_sensitive: true,
            };
            if let Ok(leaf) = table.intern(ColumnRef::AuthoritativeTimestamp, test) {
                bound_leaves.push(NormalExpr::Leaf {
                    leaf,
                    is_negated: false,
                });
            }
        }

        let mut children = match &self.root {
            NormalExpr::MatchAll => Vec::new(),
            NormalExpr::And(children) => children.to_vec(),
            root => vec![root.clone()],
        };
        children.extend(bound_leaves);
        Self {
            leaves: table.into_leaves().into_boxed_slice(),
            root: build_connective(true, children),
        }
    }

    /// # Returns
    ///
    /// The root of the normalized expression.
    #[must_use]
    pub const fn root(&self) -> &NormalExpr {
        &self.root
    }

    /// # Returns
    ///
    /// The distinct leaves; [`LeafId`] `i` is element `i`.
    #[must_use]
    pub fn leaves(&self) -> &[Leaf] {
        &self.leaves
    }

    /// Evaluates the query on one record under the K3 semantics (the executable spec and the
    /// differential oracle).
    ///
    /// # Type Parameters
    ///
    /// * `ValueSourceType` - The record representation.
    ///
    /// # Returns
    ///
    /// The truth value of the root; the record matches iff it is [`TruthValue::True`].
    #[must_use]
    pub fn evaluate<ValueSourceType: RecordValueSource>(
        &self,
        source: &ValueSourceType,
    ) -> TruthValue {
        self.evaluate_node(&self.root, source)
    }

    /// Evaluates one node of the normalized tree on a record.
    ///
    /// # Type Parameters
    ///
    /// * `ValueSourceType` - The record representation.
    ///
    /// # Returns
    ///
    /// The truth value of `node`.
    fn evaluate_node<ValueSourceType: RecordValueSource>(
        &self,
        node: &NormalExpr,
        source: &ValueSourceType,
    ) -> TruthValue {
        match node {
            NormalExpr::MatchAll => TruthValue::True,
            NormalExpr::MatchNone => TruthValue::False,
            NormalExpr::And(children) => {
                let mut result = TruthValue::True;
                for child in children {
                    result = result.and(self.evaluate_node(child, source));
                    if TruthValue::False == result {
                        break;
                    }
                }
                result
            }
            NormalExpr::Or(children) => {
                let mut result = TruthValue::False;
                for child in children {
                    result = result.or(self.evaluate_node(child, source));
                    if TruthValue::True == result {
                        break;
                    }
                }
                result
            }
            NormalExpr::Leaf { leaf, is_negated } => {
                let value = usize::try_from(leaf.get())
                    .ok()
                    .and_then(|index| self.leaves.get(index))
                    .map_or(TruthValue::Unknown, |leaf| leaf.evaluate(source));
                if *is_negated { !value } else { value }
            }
        }
    }
}

/// The id of a distinct leaf of a [`Query`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LeafId(u32);

impl LeafId {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The id `value`.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// # Returns
    ///
    /// The id as an integer.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// One distinct predicate together with its precomputed comparable search types.
#[derive(Clone, Debug, PartialEq)]
pub struct Leaf {
    pub column: ColumnRef,
    pub test: PredicateTest,
    pub search_types: SearchTypeSet,
}

impl Leaf {
    /// Evaluates the predicate on one record: `Exists` is True iff some value is reached, and a
    /// comparison is Unknown if no reached value is comparable, else True iff one satisfies it.
    ///
    /// # Type Parameters
    ///
    /// * `ValueSourceType` - The record representation.
    ///
    /// # Returns
    ///
    /// The truth value of the predicate.
    #[must_use]
    pub fn evaluate<ValueSourceType: RecordValueSource>(
        &self,
        source: &ValueSourceType,
    ) -> TruthValue {
        let mut result = match self.test {
            PredicateTest::Exists => TruthValue::False,
            PredicateTest::Compare { .. } | PredicateTest::InSet { .. } => TruthValue::Unknown,
        };
        source.visit_values(&self.column, |reached| {
            let outcome = match &self.test {
                PredicateTest::Exists => Some(true),
                PredicateTest::Compare {
                    operator,
                    value,
                    is_case_sensitive,
                } => compare(value, *operator, *is_case_sensitive, reached),
                PredicateTest::InSet {
                    values,
                    is_case_sensitive,
                } => equals_any(values, *is_case_sensitive, reached),
            };
            match outcome {
                Some(true) => {
                    result = TruthValue::True;
                    false
                }
                Some(false) => {
                    result = TruthValue::False;
                    true
                }
                None => true,
            }
        });
        result
    }
}

/// NNF expression over a query's leaves; negation exists only as leaf polarity.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum NormalExpr {
    MatchAll,
    MatchNone,
    And(Box<[Self]>),
    Or(Box<[Self]>),
    Leaf { leaf: LeafId, is_negated: bool },
}

#[cfg(test)]
mod tests {
    use super::TimeBounds;

    #[test]
    fn time_bounds_convert_milliseconds_to_nanoseconds() {
        let bounds = TimeBounds::from_epoch_millis(Some(1_735_694_600_000), Some(-5));
        assert_eq!(bounds.begin_epoch_ns, Some(1_735_694_600_000_000_000));
        assert_eq!(bounds.end_epoch_ns, Some(-5_000_000));
        assert!(!bounds.is_unbounded());
        assert!(TimeBounds::from_epoch_millis(None, None).is_unbounded());
    }

    #[test]
    fn time_bounds_saturate_instead_of_overflowing() {
        let bounds = TimeBounds::from_epoch_millis(Some(i64::MIN), Some(i64::MAX));
        assert_eq!(bounds.begin_epoch_ns, Some(i64::MIN));
        assert_eq!(bounds.end_epoch_ns, Some(i64::MAX));
    }
}
