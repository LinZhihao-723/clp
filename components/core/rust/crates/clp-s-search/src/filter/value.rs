//! Pre-coerced value tests: a leaf's comparison turned, once per filter, into key ranges over an
//! ordered domain (integers, floats, timestamps) or per-value truth values, so kernels never
//! re-coerce the literal per row.
//!
//! The ranges are derived from the reference comparison itself (by bisection over the monotone
//! predicate), so they agree with the reference evaluator on every value by construction.

use clp_s_query::CompareOperator;
use clp_s_query::Leaf;
use clp_s_query::PredicateTest;
use clp_s_query::QueryValue;
use clp_s_query::ScalarValue;
use clp_s_query::TruthValue;

use crate::outcome::SingleValue;

/// An ordered domain of stored values, mapped onto `i64` keys whose order is the value order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyDomain {
    /// `i64` values; the key is the value.
    Integer,

    /// `f64` values; the key is [`float_key`] (NaN keys lie outside every range).
    Float,

    /// Epoch nanoseconds; the key is the value.
    Timestamp,
}

impl KeyDomain {
    /// # Returns
    ///
    /// The smallest and the largest key of a non-NaN value.
    const fn bounds(self) -> (i64, i64) {
        match self {
            Self::Integer | Self::Timestamp => (i64::MIN, i64::MAX),
            Self::Float => (float_key(f64::NEG_INFINITY), float_key(f64::INFINITY)),
        }
    }

    /// # Returns
    ///
    /// The value of `key`.
    const fn value(self, key: i64) -> ScalarValue<'static> {
        match self {
            Self::Integer => ScalarValue::Integer(key),
            Self::Float => ScalarValue::Float(float_of_key(key)),
            Self::Timestamp => ScalarValue::Timestamp { epoch_ns: key },
        }
    }
}

/// The outcome of a leaf on every value of one [`KeyDomain`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyTest {
    /// Every value takes this truth value (`Unknown` when the literal is not comparable with the
    /// domain).
    Constant(TruthValue),

    /// A value is True iff its key lies in one of these sorted, disjoint, inclusive ranges, else
    /// False.
    Ranges(Box<[KeyRange]>),
}

impl KeyTest {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The outcomes of `leaf` on the values of `domain`.
    pub fn of_leaf(leaf: &Leaf, domain: KeyDomain) -> Self {
        match &leaf.test {
            PredicateTest::Exists => Self::Constant(TruthValue::True),
            PredicateTest::Compare {
                operator,
                value,
                is_case_sensitive,
            } => {
                let probe = truth_at(leaf, domain, domain.bounds().0);
                if TruthValue::Unknown == probe {
                    return Self::Constant(TruthValue::Unknown);
                }
                if QueryValue::Null == *value {
                    return Self::Constant(TruthValue::False);
                }
                Self::Ranges(
                    comparison_range(*operator, value, *is_case_sensitive, domain)
                        .into_iter()
                        .collect(),
                )
            }
            PredicateTest::InSet {
                values,
                is_case_sensitive,
            } => {
                let mut is_any_comparable = false;
                let mut ranges = Vec::new();
                for value in values {
                    let equality = comparison(CompareOperator::Equal, value, *is_case_sensitive);
                    if TruthValue::Unknown == truth_at(&equality, domain, domain.bounds().0) {
                        continue;
                    }
                    is_any_comparable = true;
                    if QueryValue::Null == *value {
                        continue;
                    }
                    ranges.extend(comparison_range(
                        CompareOperator::Equal,
                        value,
                        *is_case_sensitive,
                        domain,
                    ));
                }
                if is_any_comparable {
                    Self::Ranges(merge(ranges))
                } else {
                    Self::Constant(TruthValue::Unknown)
                }
            }
            _ => Self::Constant(TruthValue::Unknown),
        }
    }

    /// # Returns
    ///
    /// The outcome of the value with `key`.
    pub fn outcome(&self, key: i64) -> TruthValue {
        match self {
            Self::Constant(value) => *value,
            Self::Ranges(ranges) => truth(contains(ranges, key)),
        }
    }
}

/// An inclusive range of keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyRange {
    pub first: i64,
    pub last: i64,
}

/// The outcome of a leaf on one value of any type (an element of an unstructured array), with
/// the numeric and constant cases pre-coerced.
#[derive(Debug)]
pub struct ElementTest {
    leaf: Leaf,
    integer: KeyTest,
    float: KeyTest,
    on_true: TruthValue,
    on_false: TruthValue,
    on_null: TruthValue,
    on_container: TruthValue,
}

impl ElementTest {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The element test of `leaf`.
    pub fn new(leaf: &Leaf) -> Self {
        let at = |value: ScalarValue<'_>| SingleValue(value).evaluate(leaf);
        Self {
            leaf: leaf.clone(),
            integer: KeyTest::of_leaf(leaf, KeyDomain::Integer),
            float: KeyTest::of_leaf(leaf, KeyDomain::Float),
            on_true: at(ScalarValue::Boolean(true)),
            on_false: at(ScalarValue::Boolean(false)),
            on_null: at(ScalarValue::Null),
            on_container: at(ScalarValue::Object),
        }
    }

    /// # Returns
    ///
    /// Whether the leaf is `Exists`, so every reached value is True.
    pub const fn is_exists(&self) -> bool {
        matches!(self.leaf.test, PredicateTest::Exists)
    }

    /// # Returns
    ///
    /// The outcome of the leaf on the single value `value`.
    pub fn outcome(&self, value: ScalarValue<'_>) -> TruthValue {
        match value {
            ScalarValue::Integer(value) => self.integer.outcome(value),
            ScalarValue::Float(value) => self.float.outcome(float_key(value)),
            ScalarValue::Boolean(true) => self.on_true,
            ScalarValue::Boolean(false) => self.on_false,
            ScalarValue::Null => self.on_null,
            ScalarValue::Object | ScalarValue::EmptyContainer => self.on_container,
            ScalarValue::Text(_) | ScalarValue::Timestamp { .. } => {
                SingleValue(value).evaluate(&self.leaf)
            }
        }
    }
}

/// Maps a float onto an `i64` whose order is the IEEE total order (`-NaN < -inf < ... < -0 < +0
/// < ... < +inf < +NaN`).
///
/// # Returns
///
/// The key of `value`.
pub const fn float_key(value: f64) -> i64 {
    let bits = value.to_bits().cast_signed();
    if bits < 0 { bits ^ i64::MAX } else { bits }
}

/// # Returns
///
/// Whether `key` lies in one of the sorted, disjoint `ranges`.
pub fn contains(ranges: &[KeyRange], key: i64) -> bool {
    match ranges {
        [] => false,
        [range] => range.first <= key && key <= range.last,
        _ => {
            let position = ranges.partition_point(|range| range.last < key);
            ranges.get(position).is_some_and(|range| range.first <= key)
        }
    }
}

/// # Returns
///
/// True or False.
pub const fn truth(value: bool) -> TruthValue {
    if value {
        TruthValue::True
    } else {
        TruthValue::False
    }
}

/// # Returns
///
/// The float of `key` (the inverse of [`float_key`]).
const fn float_of_key(key: i64) -> f64 {
    let bits = if key < 0 { key ^ i64::MAX } else { key };
    f64::from_bits(bits.cast_unsigned())
}

/// # Returns
///
/// The leaf comparing a value with `value` under `operator` (its column is irrelevant: it is
/// only evaluated on single values).
fn comparison(operator: CompareOperator, value: &QueryValue, is_case_sensitive: bool) -> Leaf {
    Leaf {
        column: clp_s_query::ColumnRef::AnyLeaf,
        test: PredicateTest::Compare {
            operator,
            value: value.clone(),
            is_case_sensitive,
        },
        search_types: value.comparable_search_types(operator),
    }
}

/// # Returns
///
/// The truth value of `leaf` on the value of `key` in `domain`.
fn truth_at(leaf: &Leaf, domain: KeyDomain, key: i64) -> TruthValue {
    SingleValue(domain.value(key)).evaluate(leaf)
}

/// Derives the keys of `domain` satisfying a comparison with a non-null literal comparable with
/// the domain. Ordering operators are monotone in the key, so their sets are rays found by
/// bisection; an equality is the intersection of its `>=` and `<=` rays.
///
/// # Returns
///
/// The range of satisfying keys, or `None` if no key satisfies the comparison.
fn comparison_range(
    operator: CompareOperator,
    value: &QueryValue,
    is_case_sensitive: bool,
    domain: KeyDomain,
) -> Option<KeyRange> {
    let (first_key, last_key) = domain.bounds();
    let holds = |operator: CompareOperator| {
        let leaf = comparison(operator, value, is_case_sensitive);
        move |key: i64| TruthValue::True == truth_at(&leaf, domain, key)
    };
    let (first, last) = match operator {
        CompareOperator::Greater | CompareOperator::GreaterOrEqual => {
            (first_true(first_key, last_key, holds(operator))?, last_key)
        }
        CompareOperator::Less | CompareOperator::LessOrEqual => {
            (first_key, last_true(first_key, last_key, holds(operator))?)
        }
        CompareOperator::Equal => (
            first_true(first_key, last_key, holds(CompareOperator::GreaterOrEqual))?,
            last_true(first_key, last_key, holds(CompareOperator::LessOrEqual))?,
        ),
    };
    (first <= last).then_some(KeyRange { first, last })
}

/// # Type Parameters
///
/// * `PredicateType` - A predicate that is False below some key and True from it on.
///
/// # Returns
///
/// The smallest key of `[first, last]` on which `holds` is True, or `None` if there is none.
fn first_true<PredicateType: Fn(i64) -> bool>(
    first: i64,
    last: i64,
    holds: PredicateType,
) -> Option<i64> {
    if !holds(last) {
        return None;
    }
    if holds(first) {
        return Some(first);
    }
    let (mut below, mut at) = (i128::from(first), i128::from(last));
    while at - below > 1 {
        let middle = below + (at - below) / 2;
        let key = i64::try_from(middle).ok()?;
        if holds(key) {
            at = middle;
        } else {
            below = middle;
        }
    }
    i64::try_from(at).ok()
}

/// # Type Parameters
///
/// * `PredicateType` - A predicate that is True up to some key and False after it.
///
/// # Returns
///
/// The largest key of `[first, last]` on which `holds` is True, or `None` if there is none.
fn last_true<PredicateType: Fn(i64) -> bool>(
    first: i64,
    last: i64,
    holds: PredicateType,
) -> Option<i64> {
    if !holds(first) {
        return None;
    }
    if holds(last) {
        return Some(last);
    }
    let (mut at, mut above) = (i128::from(first), i128::from(last));
    while above - at > 1 {
        let middle = at + (above - at) / 2;
        let key = i64::try_from(middle).ok()?;
        if holds(key) {
            at = middle;
        } else {
            above = middle;
        }
    }
    i64::try_from(at).ok()
}

/// # Returns
///
/// `ranges` sorted, with overlapping and adjacent ranges merged.
fn merge(mut ranges: Vec<KeyRange>) -> Box<[KeyRange]> {
    ranges.sort_by_key(|range| range.first);
    let mut merged: Vec<KeyRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.first <= last.last.saturating_add(1) => {
                last.last = last.last.max(range.last);
            }
            _ => merged.push(range),
        }
    }
    merged.into_boxed_slice()
}

#[cfg(test)]
mod tests {
    use clp_s_query::Query;
    use clp_s_query::QueryOptions;
    use clp_s_query::ScalarValue;
    use clp_s_query::TruthValue;

    use super::ElementTest;
    use super::KeyDomain;
    use super::KeyRange;
    use super::KeyTest;
    use super::float_key;
    use super::float_of_key;
    use crate::outcome::SingleValue;

    /// # Returns
    ///
    /// The single leaf of `kql`.
    fn leaf(kql: &str) -> clp_s_query::Leaf {
        let query = Query::from_kql(kql, &QueryOptions::default()).expect("the query should parse");
        let [leaf] = query.leaves() else {
            panic!("{kql} should have one leaf, not {:?}", query.leaves());
        };
        leaf.clone()
    }

    /// Asserts that the key test of `kql` agrees with the reference comparison on every value.
    fn assert_agrees(kql: &str, domain: KeyDomain, values: &[ScalarValue<'_>]) {
        let leaf = leaf(kql);
        let test = KeyTest::of_leaf(&leaf, domain);
        for value in values {
            let key = match *value {
                ScalarValue::Integer(value) => value,
                ScalarValue::Float(value) => float_key(value),
                ScalarValue::Timestamp { epoch_ns } => epoch_ns,
                _ => panic!("{value:?} is not in a key domain"),
            };
            assert_eq!(
                test.outcome(key),
                SingleValue(*value).evaluate(&leaf),
                "{kql} on {value:?} ({test:?})"
            );
        }
    }

    const INTEGERS: [i64; 13] = [
        i64::MIN,
        i64::MIN + 1,
        -1_000_001,
        -6,
        -5,
        -1,
        0,
        1,
        4,
        5,
        6,
        i64::MAX - 1,
        i64::MAX,
    ];

    #[test]
    fn float_keys_order_like_the_values() {
        let values = [
            f64::NEG_INFINITY,
            -1e300,
            -1.5,
            -5e-324,
            -0.0,
            0.0,
            5e-324,
            1.0,
            1.5,
            f64::MAX,
            f64::INFINITY,
        ];
        for pair in values.windows(2) {
            assert!(float_key(pair[0]) < float_key(pair[1]), "{pair:?}");
        }
        for value in values {
            assert_eq!(float_of_key(float_key(value)).to_bits(), value.to_bits());
        }
        assert!(float_key(f64::NAN) > float_key(f64::INFINITY));
        assert!(float_key(-f64::NAN) < float_key(f64::NEG_INFINITY));
    }

    #[test]
    fn integer_ranges_agree_with_the_reference() {
        let values: Vec<ScalarValue<'_>> = INTEGERS.into_iter().map(ScalarValue::Integer).collect();
        for kql in [
            "a: 5",
            "a > 5",
            "a >= 5",
            "a < 5",
            "a <= -6",
            "a: 5.0",
            "a: 5.5",
            "a > 4.5",
            "a < 4.5",
            "a >= -5.5",
            "a <= -5.5",
            "a > 1e30",
            "a < 1e30",
            "a > -1e30",
            "a: 9223372036854775807",
            "a: -9223372036854775808",
            "a: (1 5 6 9)",
            "a: (4 5 6)",
            "a: null",
            "a: foo",
            "a: true",
            r#"a > timestamp("5", "\E")"#,
        ] {
            assert_agrees(kql, KeyDomain::Integer, &values);
        }

        let mixed = clp_s_query::Leaf {
            test: clp_s_query::PredicateTest::InSet {
                values: Box::new([
                    clp_s_query::QueryValue::Integer {
                        value: 5,
                        text: "5".into(),
                    },
                    clp_s_query::QueryValue::Text(clp_s_query::TextPattern::new(vec![
                        clp_s_query::PatternPiece::Literal(b"foo".as_slice().into()),
                    ])),
                    clp_s_query::QueryValue::Null,
                ]),
                is_case_sensitive: true,
            },
            ..leaf("a: 5")
        };
        let test = KeyTest::of_leaf(&mixed, KeyDomain::Integer);
        for value in INTEGERS {
            assert_eq!(
                test.outcome(value),
                SingleValue(ScalarValue::Integer(value)).evaluate(&mixed),
                "{value}"
            );
        }
        assert_eq!(
            test,
            KeyTest::Ranges(Box::new([KeyRange { first: 5, last: 5 }]))
        );
    }

    #[test]
    fn float_ranges_agree_with_the_reference() {
        let values: Vec<ScalarValue<'_>> = [
            f64::NEG_INFINITY,
            -1e30,
            -5.5,
            -5.0,
            -0.0,
            0.0,
            4.999_999_999_999_999,
            5.0,
            5.000_000_000_000_001,
            5.5,
            9_007_199_254_740_992.0,
            9_223_372_036_854_775_807.0,
            1e30,
            f64::INFINITY,
            f64::NAN,
        ]
        .into_iter()
        .map(ScalarValue::Float)
        .collect();
        for kql in [
            "a: 5",
            "a: 0",
            "a > 5",
            "a <= 5",
            "a: 5.5",
            "a < 5.5",
            "a >= -5.5",
            "a: 9007199254740993",
            "a: 9223372036854775807",
            "a > 9223372036854775806",
            "a: (0 5.5 7)",
            "a: null",
            "a: foo",
            r#"a: timestamp("5", "\E")"#,
        ] {
            assert_agrees(kql, KeyDomain::Float, &values);
        }
    }

    #[test]
    fn timestamp_ranges_compare_integer_literals_in_milliseconds() {
        let values: Vec<ScalarValue<'_>> = [
            i64::MIN,
            -1,
            0,
            1_699_999_999_999_999_999,
            1_700_000_000_000_000_000,
            1_700_000_000_000_000_001,
            1_700_000_000_001_000_000,
            i64::MAX,
        ]
        .into_iter()
        .map(|epoch_ns| ScalarValue::Timestamp { epoch_ns })
        .collect();
        for kql in [
            "t: 1700000000000",
            "t > 1700000000000",
            "t <= 1700000000000",
            "t: 1700000000000.0000001",
            "t >= 1699999999999.9999",
            "t > 9223372036854775807",
            "t < -9223372036854775808",
            r#"t >= timestamp("1700000000", "\E")"#,
            "t: (0 1700000000001)",
            "t: null",
            "t: foo",
        ] {
            assert_agrees(kql, KeyDomain::Timestamp, &values);
        }
        assert_eq!(
            KeyTest::of_leaf(&leaf("t: 1700000000000"), KeyDomain::Timestamp),
            KeyTest::Ranges(Box::new([KeyRange {
                first: 1_700_000_000_000_000_000,
                last: 1_700_000_000_000_000_000,
            }]))
        );
    }

    #[test]
    fn constant_outcomes_cover_exists_null_and_incomparable_literals() {
        assert_eq!(
            KeyTest::of_leaf(&leaf("a: *"), KeyDomain::Float),
            KeyTest::Constant(TruthValue::True)
        );
        assert_eq!(
            KeyTest::of_leaf(&leaf("a: null"), KeyDomain::Integer),
            KeyTest::Constant(TruthValue::False)
        );
        assert_eq!(
            KeyTest::of_leaf(&leaf("a: \"x y\""), KeyDomain::Integer),
            KeyTest::Constant(TruthValue::Unknown)
        );
        assert_eq!(
            KeyTest::of_leaf(&leaf("a: (1 3 2 4)"), KeyDomain::Integer),
            KeyTest::Ranges(Box::new([KeyRange { first: 1, last: 4 }]))
        );
    }

    #[test]
    fn element_tests_agree_with_the_reference_on_every_type() {
        let values = [
            ScalarValue::Integer(5),
            ScalarValue::Integer(-1),
            ScalarValue::Float(5.0),
            ScalarValue::Float(0.5),
            ScalarValue::Text(b"5"),
            ScalarValue::Text(b"null"),
            ScalarValue::Text(b"TRUE"),
            ScalarValue::Text(b"hello world"),
            ScalarValue::Boolean(true),
            ScalarValue::Boolean(false),
            ScalarValue::Null,
            ScalarValue::Object,
            ScalarValue::EmptyContainer,
        ];
        for (kql, is_case_sensitive) in [
            ("a: 5", true),
            ("a > 0", true),
            ("a: null", true),
            ("a: true", false),
            ("a: hello*", true),
            ("a: *", true),
            ("a: (5 null)", true),
        ] {
            let options = QueryOptions::default().with_case_sensitivity(is_case_sensitive);
            let query = Query::from_kql(kql, &options).expect("the query should parse");
            let leaf = &query.leaves()[0];
            let test = ElementTest::new(leaf);
            for value in values {
                assert_eq!(
                    test.outcome(value),
                    SingleValue(value).evaluate(leaf),
                    "{kql} on {value:?}"
                );
            }
        }
    }
}
