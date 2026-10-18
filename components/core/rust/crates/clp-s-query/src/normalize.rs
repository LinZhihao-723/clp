//! Normalization of a [`LogicalExpr`] into negation normal form over a table of distinct leaves.
//!
//! Every pass is iterative. Constants fold under Kleene's logic; a subexpression that is Unknown
//! for every record folds to False, which is exact for matching because in negation normal form a
//! record matches iff the root is True, and an Unknown operand never contributes to a True AND or
//! OR.

use std::collections::HashMap;
use std::collections::HashSet;

use crate::ColumnRef;
use crate::CompareOperator;
use crate::Leaf;
use crate::LeafId;
use crate::LogicalExpr;
use crate::NormalExpr;
use crate::Predicate;
use crate::PredicateTest;
use crate::QueryError;
use crate::QueryLimits;
use crate::QueryValue;
use crate::SearchTypeSet;
use crate::TextPattern;
use crate::TruthValue;

/// The table of distinct leaves being built.
pub struct LeafTable {
    leaves: Vec<Leaf>,
    ids: HashMap<(ColumnRef, TestKey), LeafId>,
    max_leaves: u32,
}

impl LeafTable {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// An empty table that admits at most `max_leaves` leaves.
    pub fn new(max_leaves: u32) -> Self {
        Self {
            leaves: Vec::new(),
            ids: HashMap::new(),
            max_leaves,
        }
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// A table that already holds `leaves` (which must be distinct).
    pub fn from_leaves(leaves: Vec<Leaf>) -> Self {
        let ids = leaves
            .iter()
            .zip(0_u32..)
            .map(|(leaf, id)| {
                (
                    (leaf.column.clone(), TestKey::of(&leaf.test)),
                    LeafId::new(id),
                )
            })
            .collect();
        Self {
            leaves,
            ids,
            max_leaves: u32::MAX,
        }
    }

    /// # Returns
    ///
    /// The leaves, indexed by [`LeafId`].
    pub fn into_leaves(self) -> Vec<Leaf> {
        self.leaves
    }

    /// # Returns
    ///
    /// The id of the leaf `(column, test)`, adding it if it is new, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`QueryError::LimitExceeded`] if a new leaf would exceed the leaf limit.
    pub fn intern(&mut self, column: ColumnRef, test: PredicateTest) -> Result<LeafId, QueryError> {
        let key = (column, TestKey::of(&test));
        if let Some(&id) = self.ids.get(&key) {
            return Ok(id);
        }
        let num_leaves = u32::try_from(self.leaves.len()).unwrap_or(u32::MAX);
        if num_leaves >= self.max_leaves {
            return Err(QueryError::LimitExceeded {
                what: "distinct leaves",
                actual: u64::from(num_leaves) + 1,
                limit: u64::from(self.max_leaves),
            });
        }
        let id = LeafId::new(num_leaves);
        self.leaves.push(new_leaf(key.0.clone(), test));
        self.ids.insert(key, id);
        Ok(id)
    }

    /// Converts a normalized tree, interning its leaves and dropping repeated leaf occurrences
    /// under one connective.
    ///
    /// # Returns
    ///
    /// The interned expression on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::intern`]'s return values on failure.
    fn intern_tree(&mut self, node: Node) -> Result<NormalExpr, QueryError> {
        enum Step {
            Visit(Node),
            Build { is_and: bool, num_children: usize },
        }

        let mut steps = vec![Step::Visit(node)];
        let mut built: Vec<NormalExpr> = Vec::new();
        while let Some(step) = steps.pop() {
            match step {
                Step::Visit(Node::Constant(value)) => built.push(if value {
                    NormalExpr::MatchAll
                } else {
                    NormalExpr::MatchNone
                }),
                Step::Visit(Node::Leaf {
                    column,
                    test,
                    is_negated,
                }) => {
                    let leaf = self.intern(column, test)?;
                    built.push(NormalExpr::Leaf { leaf, is_negated });
                }
                Step::Visit(Node::And(children)) => {
                    steps.push(Step::Build {
                        is_and: true,
                        num_children: children.len(),
                    });
                    steps.extend(children.into_iter().rev().map(Step::Visit));
                }
                Step::Visit(Node::Or(children)) => {
                    steps.push(Step::Build {
                        is_and: false,
                        num_children: children.len(),
                    });
                    steps.extend(children.into_iter().rev().map(Step::Visit));
                }
                Step::Build {
                    is_and,
                    num_children,
                } => {
                    let children = built.split_off(built.len().saturating_sub(num_children));
                    built.push(build_connective(is_and, children));
                }
            }
        }
        Ok(built.pop().unwrap_or(NormalExpr::MatchNone))
    }
}

/// Normalizes `expr`.
///
/// # Returns
///
/// The distinct leaves and the root on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`QueryError::LimitExceeded`] if `expr` is deeper than `limits.max_depth` or has more distinct
///   leaves than `limits.max_leaves`.
pub fn normalize(
    expr: &LogicalExpr,
    limits: QueryLimits,
) -> Result<(Vec<Leaf>, NormalExpr), QueryError> {
    let depth = expr.depth();
    if depth > u64::from(limits.max_depth) {
        return Err(QueryError::LimitExceeded {
            what: "nesting levels",
            actual: depth,
            limit: u64::from(limits.max_depth),
        });
    }
    let node = to_negation_normal_form(expr);
    let mut table = LeafTable::new(limits.max_leaves);
    let root = table.intern_tree(node)?;
    Ok((table.leaves, root))
}

/// # Returns
///
/// A leaf with its comparable search types precomputed.
pub fn new_leaf(column: ColumnRef, test: PredicateTest) -> Leaf {
    let search_types = match &test {
        PredicateTest::Exists => SearchTypeSet::ALL,
        PredicateTest::Compare {
            operator, value, ..
        } => value.comparable_search_types(*operator),
        PredicateTest::InSet { values, .. } => {
            values.iter().fold(SearchTypeSet::EMPTY, |set, value| {
                set.union(value.comparable_search_types(CompareOperator::Equal))
            })
        }
    };
    Leaf {
        column,
        test,
        search_types,
    }
}

/// # Returns
///
/// The AND (or OR) of interned children, without repeated leaf occurrences.
pub fn build_connective(is_and: bool, children: Vec<NormalExpr>) -> NormalExpr {
    let mut seen = HashSet::new();
    let mut children: Vec<NormalExpr> = children
        .into_iter()
        .filter(|child| match child {
            NormalExpr::Leaf { leaf, is_negated } => seen.insert((*leaf, *is_negated)),
            _ => true,
        })
        .collect();
    match children.len() {
        0 if is_and => NormalExpr::MatchAll,
        0 => NormalExpr::MatchNone,
        1 => children.pop().unwrap_or(NormalExpr::MatchNone),
        _ if is_and => NormalExpr::And(children.into_boxed_slice()),
        _ => NormalExpr::Or(children.into_boxed_slice()),
    }
}

/// An expression in negation normal form, before leaf interning.
enum Node {
    Constant(bool),
    And(Vec<Self>),
    Or(Vec<Self>),
    Leaf {
        column: ColumnRef,
        test: PredicateTest,
        is_negated: bool,
    },
}

/// The members that one in-set leaf collects.
struct InSetGroup {
    column: ColumnRef,
    is_case_sensitive: bool,
    is_negated: bool,
    values: Vec<QueryValue>,
    seen: HashSet<ValueKey>,
}

impl InSetGroup {
    /// # Returns
    ///
    /// The leaf of the group: an equality for one member, else an in-set test.
    fn into_node(mut self) -> Node {
        let test = if 1 == self.values.len() {
            PredicateTest::Compare {
                operator: CompareOperator::Equal,
                value: self.values.remove(0),
                is_case_sensitive: self.is_case_sensitive,
            }
        } else {
            PredicateTest::InSet {
                values: self.values.into_boxed_slice(),
                is_case_sensitive: self.is_case_sensitive,
            }
        };
        Node::Leaf {
            column: self.column,
            test,
            is_negated: self.is_negated,
        }
    }
}

/// A hashable identity of a [`QueryValue`]; floats are identified by their bits and text.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ValueKey {
    Integer(i64, Box<str>),
    Float(u64, Box<str>),
    Boolean(bool),
    Null,
    Text(TextPattern),
    Timestamp(i64),
}

impl ValueKey {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The identity of `value`.
    fn of(value: &QueryValue) -> Self {
        match value {
            QueryValue::Integer { value, text } => Self::Integer(*value, text.clone()),
            QueryValue::Float { value, text } => Self::Float(value.to_bits(), text.clone()),
            QueryValue::Boolean(value) => Self::Boolean(*value),
            QueryValue::Null => Self::Null,
            QueryValue::Text(pattern) => Self::Text(pattern.clone()),
            QueryValue::Timestamp { epoch_ns } => Self::Timestamp(*epoch_ns),
        }
    }
}

/// A hashable identity of a [`PredicateTest`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum TestKey {
    Exists,
    Compare(CompareOperator, ValueKey, bool),
    InSet(Box<[ValueKey]>, bool),
}

impl TestKey {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The identity of `test`.
    fn of(test: &PredicateTest) -> Self {
        match test {
            PredicateTest::Exists => Self::Exists,
            PredicateTest::Compare {
                operator,
                value,
                is_case_sensitive,
            } => Self::Compare(*operator, ValueKey::of(value), *is_case_sensitive),
            PredicateTest::InSet {
                values,
                is_case_sensitive,
            } => Self::InSet(
                values.iter().map(ValueKey::of).collect(),
                *is_case_sensitive,
            ),
        }
    }
}

/// Pushes negations to the leaves (De Morgan), flattens, folds constants and folds ORs of
/// equalities into in-set tests.
///
/// # Returns
///
/// The normalized tree.
fn to_negation_normal_form(expr: &LogicalExpr) -> Node {
    enum Step<'expr> {
        Visit {
            expr: &'expr LogicalExpr,
            is_negated: bool,
        },
        Combine {
            is_and: bool,
            num_children: usize,
        },
    }

    let mut steps = vec![Step::Visit {
        expr,
        is_negated: false,
    }];
    let mut built: Vec<Node> = Vec::new();
    while let Some(step) = steps.pop() {
        match step {
            Step::Visit { expr, is_negated } => match expr {
                LogicalExpr::Constant(value) => {
                    let value = if is_negated { !*value } else { *value };
                    built.push(Node::Constant(TruthValue::True == value));
                }
                LogicalExpr::Not(child) => steps.push(Step::Visit {
                    expr: child,
                    is_negated: !is_negated,
                }),
                LogicalExpr::And(children) | LogicalExpr::Or(children) => {
                    steps.push(Step::Combine {
                        is_and: matches!(expr, LogicalExpr::And(_)) != is_negated,
                        num_children: children.len(),
                    });
                    steps.extend(children.iter().rev().map(|child| Step::Visit {
                        expr: child,
                        is_negated,
                    }));
                }
                LogicalExpr::Predicate(predicate) => built.push(leaf_node(predicate, is_negated)),
            },
            Step::Combine {
                is_and,
                num_children,
            } => {
                let children = built.split_off(built.len().saturating_sub(num_children));
                built.push(combine(is_and, children));
            }
        }
    }
    built.pop().unwrap_or(Node::Constant(false))
}

/// # Returns
///
/// The node of a predicate under a polarity, folded to a constant when its value does not depend
/// on the record.
fn leaf_node(predicate: &Predicate, is_negated: bool) -> Node {
    match (&predicate.column, &predicate.test) {
        // `*` matches every record (C++ parity), even one without leaves.
        (ColumnRef::AnyLeaf, PredicateTest::Exists) => Node::Constant(!is_negated),

        // Unknown for every record, in both polarities.
        (
            _,
            PredicateTest::Compare {
                operator, value, ..
            },
        ) if value.comparable_search_types(*operator).is_empty() => Node::Constant(false),

        // An OR of no equalities is False.
        (_, PredicateTest::InSet { values, .. }) if values.is_empty() => Node::Constant(is_negated),

        (column, test) => Node::Leaf {
            column: column.clone(),
            test: test.clone(),
            is_negated,
        },
    }
}

/// # Returns
///
/// The AND (or OR) of normalized children, flattened and folded.
fn combine(is_and: bool, children: Vec<Node>) -> Node {
    let mut flattened = Vec::with_capacity(children.len());
    for child in children {
        match child {
            Node::Constant(value) if value == is_and => {}
            Node::Constant(value) => return Node::Constant(value),
            Node::And(grandchildren) if is_and => flattened.extend(grandchildren),
            Node::Or(grandchildren) if !is_and => flattened.extend(grandchildren),
            child => flattened.push(child),
        }
    }
    let mut folded = fold_in_sets(is_and, flattened);
    match folded.len() {
        0 => Node::Constant(is_and),
        1 => folded.pop().unwrap_or(Node::Constant(is_and)),
        _ if is_and => Node::And(folded),
        _ => Node::Or(folded),
    }
}

/// Folds the equalities on one column among an OR's children (or the negated equalities among an
/// AND's children, by De Morgan) into one in-set leaf.
///
/// Only equalities with the same comparable search types and case sensitivity fold: then both
/// forms are Unknown exactly when no comparable value is reached, so the fold is exact under K3.
///
/// # Returns
///
/// The children with each foldable group replaced by its in-set leaf, at the position of the
/// group's first member.
fn fold_in_sets(is_and: bool, children: Vec<Node>) -> Vec<Node> {
    enum Slot {
        Node(Node),
        Group(usize),
    }

    let mut slots = Vec::with_capacity(children.len());
    let mut groups: Vec<InSetGroup> = Vec::new();
    let mut group_index: HashMap<(ColumnRef, bool, SearchTypeSet), usize> = HashMap::new();
    for child in children {
        let Node::Leaf {
            column,
            test,
            is_negated,
        } = child
        else {
            slots.push(Slot::Node(child));
            continue;
        };
        let Some((values, is_case_sensitive, search_types)) =
            equality_members(is_negated == is_and, &test)
        else {
            slots.push(Slot::Node(Node::Leaf {
                column,
                test,
                is_negated,
            }));
            continue;
        };
        let key = (column, is_case_sensitive, search_types);
        let index = if let Some(&index) = group_index.get(&key) {
            index
        } else {
            let index = groups.len();
            groups.push(InSetGroup {
                column: key.0.clone(),
                is_case_sensitive,
                is_negated,
                values: Vec::new(),
                seen: HashSet::new(),
            });
            group_index.insert(key, index);
            slots.push(Slot::Group(index));
            index
        };
        let group = &mut groups[index];
        for value in values {
            if group.seen.insert(ValueKey::of(value)) {
                group.values.push(value.clone());
            }
        }
    }

    let mut groups: Vec<Option<InSetGroup>> = groups.into_iter().map(Some).collect();
    slots
        .into_iter()
        .filter_map(|slot| match slot {
            Slot::Node(node) => Some(node),
            Slot::Group(index) => groups
                .get_mut(index)
                .and_then(Option::take)
                .map(InSetGroup::into_node),
        })
        .collect()
}

/// # Returns
///
/// The members, case sensitivity and common search types of an equality or in-set test with the
/// polarity that folds (`is_foldable_polarity`), or `None` if the test does not fold.
fn equality_members(
    is_foldable_polarity: bool,
    test: &PredicateTest,
) -> Option<(&[QueryValue], bool, SearchTypeSet)> {
    if !is_foldable_polarity {
        return None;
    }
    let (values, is_case_sensitive) = match test {
        PredicateTest::Compare {
            operator: CompareOperator::Equal,
            value,
            is_case_sensitive,
        } => (std::slice::from_ref(value), *is_case_sensitive),
        PredicateTest::InSet {
            values,
            is_case_sensitive,
        } => (&**values, *is_case_sensitive),
        _ => return None,
    };
    let (first, rest) = values.split_first()?;
    let search_types = first.comparable_search_types(CompareOperator::Equal);
    rest.iter()
        .all(|value| value.comparable_search_types(CompareOperator::Equal) == search_types)
        .then_some((values, is_case_sensitive, search_types))
}
