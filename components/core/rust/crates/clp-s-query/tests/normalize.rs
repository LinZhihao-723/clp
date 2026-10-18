//! Normalization of expressions into [`Query`]: negation normal form, K3 constant folding,
//! flattening, leaf hash-consing, in-set folding, limits and time bounds.

use clp_s_query::ColumnPattern;
use clp_s_query::ColumnRef;
use clp_s_query::CompareOperator;
use clp_s_query::LeafId;
use clp_s_query::LogicalExpr;
use clp_s_query::NormalExpr;
use clp_s_query::Predicate;
use clp_s_query::PredicateTest;
use clp_s_query::Query;
use clp_s_query::QueryError;
use clp_s_query::QueryLimits;
use clp_s_query::QueryOptions;
use clp_s_query::QueryValue;
use clp_s_query::SearchType;
use clp_s_query::SearchTypeSet;
use clp_s_query::TimeBounds;
use clp_s_query::TruthValue;

/// Normalizes `kql` into a query with the default options.
///
/// # Returns
///
/// The normalized query.
///
/// # Panics
///
/// Panics if `kql` doesn't parse or normalize.
fn query(kql: &str) -> Query {
    Query::from_kql(kql, &QueryOptions::default())
        .unwrap_or_else(|error| panic!("{kql:?}: {error}"))
}

/// # Returns
///
/// A reference to leaf `id`, negated if `is_negated`.
const fn leaf(id: u32, is_negated: bool) -> NormalExpr {
    NormalExpr::Leaf {
        leaf: LeafId::new(id),
        is_negated,
    }
}

/// # Returns
///
/// An integer value spelled as its decimal text.
fn integer(value: i64) -> QueryValue {
    QueryValue::Integer {
        value,
        text: value.to_string().into(),
    }
}

/// # Returns
///
/// The column that the pattern `text` names.
///
/// # Panics
///
/// Panics if `text` isn't a valid column pattern.
fn column(text: &str) -> ColumnRef {
    ColumnRef::from_pattern(ColumnPattern::parse(text).expect("column should parse"))
}

/// # Returns
///
/// A predicate applying `test` to the column that the pattern `column_text` names.
///
/// # Panics
///
/// Panics if `column_text` isn't a valid column pattern.
fn predicate(column_text: &str, test: PredicateTest) -> LogicalExpr {
    LogicalExpr::Predicate(Predicate {
        column: column(column_text),
        test,
    })
}

#[test]
fn negations_are_pushed_to_the_leaves() {
    let parsed = query("NOT (a: 1 AND (b: 2 OR NOT c: 3))");
    assert_eq!(
        *parsed.root(),
        NormalExpr::Or(Box::new([
            leaf(0, true),
            NormalExpr::And(Box::new([leaf(1, true), leaf(2, false)])),
        ]))
    );
    assert_eq!(*query("NOT NOT a: 1").root(), leaf(0, false));
}

#[test]
fn nested_connectives_of_one_kind_are_flattened() {
    assert_eq!(
        *query("(a: 1 AND b: 2) AND (c: 3 AND d: 4)").root(),
        NormalExpr::And(Box::new([
            leaf(0, false),
            leaf(1, false),
            leaf(2, false),
            leaf(3, false)
        ]))
    );
    assert_eq!(
        *query("NOT (a: 1 AND b: 2) OR c: 3").root(),
        NormalExpr::Or(Box::new([leaf(0, true), leaf(1, true), leaf(2, false)]))
    );
}

#[test]
fn constants_fold_under_kleene_logic() {
    for (kql, expected) in [
        ("a: (AND)", NormalExpr::MatchAll),
        ("a: ()", NormalExpr::MatchNone),
        ("NOT a: ()", NormalExpr::MatchAll),
        ("NOT a: (AND)", NormalExpr::MatchNone),
        ("b: 1 AND a: ()", NormalExpr::MatchNone),
        ("b: 1 OR a: (AND)", NormalExpr::MatchAll),
        ("b: 1 OR a: ()", leaf(0, false)),
        ("b: 1 AND a: (AND)", leaf(0, false)),
        ("*", NormalExpr::MatchAll),
        ("NOT *", NormalExpr::MatchNone),
        ("b: 1 OR *", NormalExpr::MatchAll),
        ("b: 1 AND *", leaf(0, false)),
        ("b: 1 AND NOT *", NormalExpr::MatchNone),
        ("a > foo", NormalExpr::MatchNone),
        ("NOT a > foo", NormalExpr::MatchNone),
        ("b: 1 OR NOT a > foo", leaf(0, false)),
    ] {
        assert_eq!(*query(kql).root(), expected, "{kql}");
    }
    let constant = |value| LogicalExpr::Constant(value);
    for (expr, expected) in [
        (constant(TruthValue::True), NormalExpr::MatchAll),
        (constant(TruthValue::False), NormalExpr::MatchNone),
        (constant(TruthValue::Unknown), NormalExpr::MatchNone),
        (
            LogicalExpr::Not(Box::new(constant(TruthValue::False))),
            NormalExpr::MatchAll,
        ),
        (
            LogicalExpr::Not(Box::new(constant(TruthValue::Unknown))),
            NormalExpr::MatchNone,
        ),
        (LogicalExpr::And(vec![]), NormalExpr::MatchAll),
        (LogicalExpr::Or(vec![]), NormalExpr::MatchNone),
    ] {
        let normalized =
            Query::from_expr(&expr, &QueryOptions::default()).expect("constants should normalize");
        assert_eq!(*normalized.root(), expected, "{expr:?}");
        assert_eq!(normalized.leaves(), &[]);
    }
}

#[test]
fn equal_leaves_share_one_id_and_repeat_once_per_connective() {
    let parsed = query("a: 1 AND (b: 2 OR a: 1) AND a: 1");
    assert_eq!(parsed.leaves().len(), 2);
    assert_eq!(
        *parsed.root(),
        NormalExpr::And(Box::new([
            leaf(0, false),
            NormalExpr::Or(Box::new([leaf(1, false), leaf(0, false)])),
        ]))
    );
    assert_eq!(*query("a: 1 AND a: 1").root(), leaf(0, false));
    let negated = query("a: 1 AND NOT a: 1");
    assert_eq!(negated.leaves().len(), 1);
    assert_eq!(
        *negated.root(),
        NormalExpr::And(Box::new([leaf(0, false), leaf(0, true)]))
    );
}

#[test]
fn ors_of_equalities_on_one_column_fold_into_in_sets() {
    let parsed = query("a: (1 2 3) OR a: 2 OR b: 1");
    assert_eq!(
        *parsed.root(),
        NormalExpr::Or(Box::new([leaf(0, false), leaf(1, false)]))
    );
    assert_eq!(
        parsed.leaves()[0].test,
        PredicateTest::InSet {
            values: Box::new([integer(1), integer(2), integer(3)]),
            is_case_sensitive: true,
        }
    );
    let negated = query("NOT a: 1 AND NOT a: 2");
    assert_eq!(*negated.root(), leaf(0, true));
    assert!(matches!(
        negated.leaves()[0].test,
        PredicateTest::InSet { .. }
    ));
    let list = query("a: (NOT 1 2)");
    assert_eq!(*list.root(), leaf(0, true));
    assert_eq!(
        *query("a: (1 1)").leaves(),
        [query("a: 1").leaves()[0].clone()]
    );
}

#[test]
fn equalities_fold_only_when_they_compare_with_the_same_types() {
    let mixed = query("a: (1 foo)");
    assert_eq!(
        *mixed.root(),
        NormalExpr::Or(Box::new([leaf(0, false), leaf(1, false)]))
    );
    let numeric = query("a: (1 2.5)");
    assert_eq!(*numeric.root(), leaf(0, false));
    let positive_in_and = query("a: 1 AND a: 2");
    assert_eq!(positive_in_and.leaves().len(), 2);
    let negated_in_or = query("NOT a: 1 OR NOT a: 2");
    assert_eq!(negated_in_or.leaves().len(), 2);
    let range = query("a > 1 OR a > 2");
    assert_eq!(range.leaves().len(), 2);

    let equal = |value: i64, is_case_sensitive| {
        predicate(
            "a",
            PredicateTest::Compare {
                operator: CompareOperator::Equal,
                value: integer(value),
                is_case_sensitive,
            },
        )
    };
    let expr = LogicalExpr::Or(vec![equal(1, true), equal(2, false)]);
    let normalized =
        Query::from_expr(&expr, &QueryOptions::default()).expect("query should normalize");
    assert_eq!(normalized.leaves().len(), 2);
}

#[test]
fn leaves_carry_their_comparable_search_types() {
    let parsed = query("a: 5 AND b: * AND c: (x y)");
    let types: Vec<SearchTypeSet> = parsed
        .leaves()
        .iter()
        .map(|leaf| leaf.search_types)
        .collect();
    assert_eq!(
        types[0],
        SearchTypeSet::EMPTY
            .with(SearchType::Integer)
            .with(SearchType::Float)
            .with(SearchType::Timestamp)
            .with(SearchType::Text)
            .with(SearchType::Array)
    );
    assert_eq!(types[1], SearchTypeSet::ALL);
    assert_eq!(
        types[2],
        SearchTypeSet::EMPTY
            .with(SearchType::Text)
            .with(SearchType::Array)
    );
}

#[test]
fn host_built_trees_are_depth_checked_without_recursion() {
    let mut expr = predicate("a", PredicateTest::Exists);
    for _ in 0..300_000 {
        expr = LogicalExpr::Not(Box::new(expr));
    }
    assert!(matches!(
        Query::from_expr(&expr, &QueryOptions::default()),
        Err(QueryError::LimitExceeded {
            what: "nesting levels",
            actual: 300_001,
            limit: 1024,
        })
    ));
}

#[test]
fn the_leaf_limit_bounds_distinct_leaves() {
    let options = QueryOptions {
        limits: QueryLimits {
            max_leaves: 3,
            ..QueryLimits::default()
        },
        ..QueryOptions::default()
    };
    assert!(Query::from_kql("a: 1 AND b: 1 AND c: 1 AND a: 1", &options).is_ok());
    assert!(matches!(
        Query::from_kql("a: 1 AND b: 1 AND c: 1 AND d: 1", &options),
        Err(QueryError::LimitExceeded {
            what: "distinct leaves",
            actual: 4,
            limit: 3,
        })
    ));
}

#[test]
fn time_bounds_add_inclusive_timestamp_leaves() {
    let parsed = query("level: INFO");
    assert_eq!(
        parsed.with_time_bounds(TimeBounds::default()).root(),
        parsed.root()
    );

    let bounded = parsed.with_time_bounds(TimeBounds::from_epoch_millis(Some(1), Some(2)));
    assert_eq!(
        *bounded.root(),
        NormalExpr::And(Box::new([leaf(0, false), leaf(1, false), leaf(2, false)]))
    );
    assert_eq!(
        bounded.leaves()[1].column,
        ColumnRef::AuthoritativeTimestamp
    );
    assert_eq!(
        bounded.leaves()[1].test,
        PredicateTest::Compare {
            operator: CompareOperator::GreaterOrEqual,
            value: QueryValue::Timestamp {
                epoch_ns: 1_000_000
            },
            is_case_sensitive: true,
        }
    );
    assert_eq!(
        bounded.leaves()[2].test,
        PredicateTest::Compare {
            operator: CompareOperator::LessOrEqual,
            value: QueryValue::Timestamp {
                epoch_ns: 2_000_000
            },
            is_case_sensitive: true,
        }
    );

    let lower = Query::match_all().with_time_bounds(TimeBounds::from_epoch_millis(Some(5), None));
    assert_eq!(*lower.root(), leaf(0, false));
    let twice = lower.with_time_bounds(TimeBounds::from_epoch_millis(Some(5), None));
    assert_eq!(twice.leaves().len(), 1);
    assert_eq!(*twice.root(), leaf(0, false));

    let empty = parsed.with_time_bounds(TimeBounds::from_epoch_millis(Some(3), Some(2)));
    assert_eq!(*empty.root(), NormalExpr::MatchNone);
    let none = query("NOT *").with_time_bounds(TimeBounds::from_epoch_millis(Some(1), None));
    assert_eq!(*none.root(), NormalExpr::MatchNone);
    assert_eq!(*Query::match_all().root(), NormalExpr::MatchAll);
}
