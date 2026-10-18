//! The query IR as written.

use std::fmt;

use crate::ColumnRef;
use crate::QueryError;
use crate::QueryOptions;
use crate::QueryValue;

/// Kleene's strong three-valued truth value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TruthValue {
    True,
    False,
    Unknown,
}

impl TruthValue {
    /// # Returns
    ///
    /// The K3 conjunction: False if either is False, else Unknown if either is Unknown, else True.
    #[must_use]
    pub const fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::False, _) | (_, Self::False) => Self::False,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (Self::True, Self::True) => Self::True,
        }
    }

    /// # Returns
    ///
    /// The K3 disjunction: True if either is True, else Unknown if either is Unknown, else False.
    #[must_use]
    pub const fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::True, _) | (_, Self::True) => Self::True,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (Self::False, Self::False) => Self::False,
        }
    }
}

impl std::ops::Not for TruthValue {
    type Output = Self;

    /// The K3 negation: True and False swap, Unknown stays.
    fn not(self) -> Self {
        match self {
            Self::True => Self::False,
            Self::False => Self::True,
            Self::Unknown => Self::Unknown,
        }
    }
}

/// A query as written: one negation form, three-valued constants and typed literals.
///
/// The parser bounds the depth with [`crate::QueryLimits::max_depth`], and
/// [`crate::Query::from_expr`] checks host-built trees iteratively before normalizing them.
/// `Clone`, `Debug`, `PartialEq` and `Drop` walk the tree with heap stacks, so no depth overflows
/// the call stack; because of the `Drop` impl, match an expression by reference rather than moving
/// fields out of it.
#[non_exhaustive]
pub enum LogicalExpr {
    Constant(TruthValue),
    And(Vec<Self>),
    Or(Vec<Self>),
    Not(Box<Self>),
    Predicate(Predicate),
}

impl LogicalExpr {
    /// Factory function.
    ///
    /// Parses KQL with the C++ grammar: AND and OR with equal precedence and left associativity,
    /// NOT binding tighter, the list syntax, nested `col: {q}`, and `istringstream`-compatible
    /// numeric literal typing.
    ///
    /// # Returns
    ///
    /// The parsed expression on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`QueryError::Syntax`] if `text` is not valid KQL.
    /// * [`QueryError::InvalidEscape`] if a literal contains an invalid escape sequence.
    /// * [`QueryError::InvalidColumn`] if a column is invalid.
    /// * [`QueryError::InvalidTimestamp`] if a timestamp literal is invalid or uses an unsupported
    ///   pattern.
    /// * [`QueryError::NamespaceInNestedQuery`] if a nested query declares a namespace.
    /// * [`QueryError::LimitExceeded`] if the nesting depth exceeds `options.limits`.
    pub fn parse_kql(text: &str, options: &QueryOptions) -> Result<Self, QueryError> {
        crate::kql::parse(text, options)
    }

    /// # Returns
    ///
    /// The depth of the tree (a constant or predicate has depth 1), computed without recursion.
    #[must_use]
    pub fn depth(&self) -> u64 {
        let mut max_depth = 0;
        let mut stack = vec![(self, 1_u64)];
        while let Some((expr, depth)) = stack.pop() {
            max_depth = max_depth.max(depth);
            match expr {
                Self::And(children) | Self::Or(children) => {
                    stack.extend(children.iter().map(|child| (child, depth + 1)));
                }
                Self::Not(child) => stack.push((child, depth + 1)),
                Self::Constant(_) | Self::Predicate(_) => {}
            }
        }
        max_depth
    }

    /// Moves this node's children onto `stack`, leaving the node childless.
    fn detach_children(&mut self, stack: &mut Vec<Self>) {
        match self {
            Self::And(children) | Self::Or(children) => stack.append(children),
            Self::Not(child) => stack.push(std::mem::replace(
                &mut **child,
                Self::Constant(TruthValue::Unknown),
            )),
            Self::Constant(_) | Self::Predicate(_) => {}
        }
    }
}

impl Clone for LogicalExpr {
    fn clone(&self) -> Self {
        enum Step<'expr> {
            Visit(&'expr LogicalExpr),
            BuildNot,
            BuildAnd(usize),
            BuildOr(usize),
        }

        let mut steps = vec![Step::Visit(self)];
        let mut built: Vec<Self> = Vec::new();
        while let Some(step) = steps.pop() {
            match step {
                Step::Visit(expr) => match expr {
                    Self::Constant(value) => built.push(Self::Constant(*value)),
                    Self::Predicate(predicate) => built.push(Self::Predicate(predicate.clone())),
                    Self::Not(child) => {
                        steps.push(Step::BuildNot);
                        steps.push(Step::Visit(child));
                    }
                    Self::And(children) | Self::Or(children) => {
                        steps.push(if matches!(expr, Self::And(_)) {
                            Step::BuildAnd(children.len())
                        } else {
                            Step::BuildOr(children.len())
                        });
                        steps.extend(children.iter().rev().map(Step::Visit));
                    }
                },
                Step::BuildNot => {
                    let child = built.pop().unwrap_or(Self::Constant(TruthValue::Unknown));
                    built.push(Self::Not(Box::new(child)));
                }
                Step::BuildAnd(num_children) => {
                    let children = built.split_off(built.len() - num_children);
                    built.push(Self::And(children));
                }
                Step::BuildOr(num_children) => {
                    let children = built.split_off(built.len() - num_children);
                    built.push(Self::Or(children));
                }
            }
        }
        built.pop().unwrap_or(Self::Constant(TruthValue::Unknown))
    }
}

impl fmt::Debug for LogicalExpr {
    /// Writes the same text as a derived non-alternate `Debug`; the alternate flag is ignored for
    /// the tree structure.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        enum Step<'expr> {
            Visit(&'expr LogicalExpr),
            Write(&'static str),
        }

        let mut steps = vec![Step::Visit(self)];
        while let Some(step) = steps.pop() {
            let expr = match step {
                Step::Write(text) => {
                    formatter.write_str(text)?;
                    continue;
                }
                Step::Visit(expr) => expr,
            };
            match expr {
                Self::Constant(value) => write!(formatter, "Constant({value:?})")?,
                Self::Predicate(predicate) => write!(formatter, "Predicate({predicate:?})")?,
                Self::Not(child) => {
                    formatter.write_str("Not(")?;
                    steps.push(Step::Write(")"));
                    steps.push(Step::Visit(child));
                }
                Self::And(children) | Self::Or(children) => {
                    formatter.write_str(if matches!(expr, Self::And(_)) {
                        "And(["
                    } else {
                        "Or(["
                    })?;
                    steps.push(Step::Write("])"));
                    for (index, child) in children.iter().enumerate().rev() {
                        steps.push(Step::Visit(child));
                        if 0 != index {
                            steps.push(Step::Write(", "));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

impl PartialEq for LogicalExpr {
    fn eq(&self, other: &Self) -> bool {
        let mut pairs = vec![(self, other)];
        while let Some(pair) = pairs.pop() {
            match pair {
                (Self::Constant(left), Self::Constant(right)) if left == right => {}
                (Self::Predicate(left), Self::Predicate(right)) if left == right => {}
                (Self::Not(left), Self::Not(right)) => pairs.push((left, right)),
                (Self::And(left), Self::And(right)) | (Self::Or(left), Self::Or(right))
                    if left.len() == right.len() =>
                {
                    pairs.extend(left.iter().zip(right.iter()));
                }
                _ => return false,
            }
        }
        true
    }
}

impl Drop for LogicalExpr {
    fn drop(&mut self) {
        let mut stack = Vec::new();
        self.detach_children(&mut stack);
        while let Some(mut expr) = stack.pop() {
            expr.detach_children(&mut stack);
        }
    }
}

/// A test on the values a column reference reaches in one record.
#[derive(Clone, Debug, PartialEq)]
pub struct Predicate {
    pub column: ColumnRef,
    pub test: PredicateTest,
}

/// A test that is existential over the reached values; there is no `NotEqual` (negation is
/// [`LogicalExpr::Not`]).
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq)]
pub enum PredicateTest {
    /// True if the column reaches any value (objects and empty containers included), else False.
    Exists,

    /// Unknown if no comparable value is reached; otherwise True if some value satisfies the
    /// comparison, else False.
    Compare {
        operator: CompareOperator,
        value: QueryValue,
        is_case_sensitive: bool,
    },

    /// An OR of equalities on one column, folded by normalization.
    InSet {
        values: Box<[QueryValue]>,
        is_case_sensitive: bool,
    },
}

/// A comparison operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CompareOperator {
    Equal,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

impl CompareOperator {
    /// # Returns
    ///
    /// Whether a value ordered `ordering` relative to the literal satisfies the operator.
    #[must_use]
    pub const fn holds(self, ordering: std::cmp::Ordering) -> bool {
        match self {
            Self::Equal => ordering.is_eq(),
            Self::Less => ordering.is_lt(),
            Self::LessOrEqual => ordering.is_le(),
            Self::Greater => ordering.is_gt(),
            Self::GreaterOrEqual => ordering.is_ge(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LogicalExpr;
    use super::TruthValue;
    use crate::ColumnRef;
    use crate::Predicate;
    use crate::PredicateTest;

    const DEEP: usize = 200_000;

    /// # Returns
    ///
    /// An existence test of any leaf.
    fn exists() -> LogicalExpr {
        LogicalExpr::Predicate(Predicate {
            column: ColumnRef::AnyLeaf,
            test: PredicateTest::Exists,
        })
    }

    /// # Returns
    ///
    /// An existence test under `depth` nested negations.
    fn deep_not_chain(depth: usize) -> LogicalExpr {
        let mut expr = exists();
        for _ in 0..depth {
            expr = LogicalExpr::Not(Box::new(expr));
        }
        expr
    }

    /// # Returns
    ///
    /// An existence test under `depth` nested conjunctions with `True`.
    fn deep_and_chain(depth: usize) -> LogicalExpr {
        let mut expr = exists();
        for _ in 0..depth {
            expr = LogicalExpr::And(vec![LogicalExpr::Constant(TruthValue::True), expr]);
        }
        expr
    }

    #[test]
    fn k3_connectives_follow_the_truth_tables() {
        use TruthValue::False;
        use TruthValue::True;
        use TruthValue::Unknown;
        assert_eq!([!True, !False, !Unknown], [False, True, Unknown]);
        for (left, right, and, or) in [
            (True, True, True, True),
            (True, False, False, True),
            (True, Unknown, Unknown, True),
            (False, False, False, False),
            (False, Unknown, False, Unknown),
            (Unknown, Unknown, Unknown, Unknown),
        ] {
            assert_eq!(left.and(right), and, "{left:?} AND {right:?}");
            assert_eq!(right.and(left), and, "{right:?} AND {left:?}");
            assert_eq!(left.or(right), or, "{left:?} OR {right:?}");
            assert_eq!(right.or(left), or, "{right:?} OR {left:?}");
        }
    }

    #[test]
    fn deep_trees_clone_compare_format_and_drop_without_recursion() {
        for expr in [deep_not_chain(DEEP), deep_and_chain(DEEP)] {
            let copy = expr.clone();
            assert!(copy == expr, "a clone should equal its source");
            assert_eq!(
                copy.depth(),
                u64::try_from(DEEP).expect("depth fits u64") + 1
            );
            let text = format!("{copy:?}");
            assert!(text.len() > DEEP, "debug output should cover every level");
            drop(copy);
            drop(expr);
        }
    }

    #[test]
    fn equality_distinguishes_shape_and_values() {
        let and = LogicalExpr::And(vec![exists(), LogicalExpr::Constant(TruthValue::True)]);
        let or = LogicalExpr::Or(vec![exists(), LogicalExpr::Constant(TruthValue::True)]);
        let shorter = LogicalExpr::And(vec![exists()]);
        let other_constant =
            LogicalExpr::And(vec![exists(), LogicalExpr::Constant(TruthValue::False)]);
        assert_eq!(and, and.clone());
        assert_ne!(and, or);
        assert_ne!(and, shorter);
        assert_ne!(and, other_constant);
        assert_ne!(deep_not_chain(3), deep_not_chain(4));
    }

    #[test]
    fn debug_matches_the_derived_format() {
        let expr = LogicalExpr::Or(vec![
            LogicalExpr::Not(Box::new(LogicalExpr::Constant(TruthValue::True))),
            LogicalExpr::And(vec![]),
            exists(),
        ]);
        assert_eq!(
            format!("{expr:?}"),
            "Or([Not(Constant(True)), And([]), Predicate(Predicate { column: AnyLeaf, test: \
             Exists })])"
        );
    }
}
