//! Comparisons of reached values with typed literals: the coercion table of design report §8.1,
//! with exact numeric comparisons.

use std::cmp::Ordering;

use crate::CompareOperator;
use crate::QueryValue;
use crate::ScalarValue;

/// Compares one reached value with a literal.
///
/// Integer and float literals compare with timestamps in epoch milliseconds (DV-15); timestamp
/// literals compare with integers in nanoseconds and with floats in seconds. Every numeric
/// comparison is exact (DV-14).
///
/// # Returns
///
/// `None` if `reached` is not comparable with `value` under `operator`, else whether it satisfies
/// the comparison.
pub fn compare(
    value: &QueryValue,
    operator: CompareOperator,
    is_case_sensitive: bool,
    reached: ScalarValue<'_>,
) -> Option<bool> {
    let ordered = |ordering: Option<Ordering>| {
        Some(ordering.is_some_and(|ordering| operator.holds(ordering)))
    };
    let is_equal = CompareOperator::Equal == operator;
    reached.search_type()?;
    match (value, reached) {
        (
            QueryValue::Integer { value, .. } | QueryValue::Timestamp { epoch_ns: value },
            ScalarValue::Integer(reached),
        ) => ordered(Some(reached.cmp(value))),
        (QueryValue::Integer { value, .. }, ScalarValue::Float(reached)) => {
            ordered(compare_ratio_with_float(i128::from(*value), 1, reached).map(Ordering::reverse))
        }
        (QueryValue::Integer { value, .. }, ScalarValue::Timestamp { epoch_ns }) => ordered(Some(
            i128::from(epoch_ns)
                .cmp(&(i128::from(*value) * i128::from(NANOSECONDS_PER_MILLISECOND))),
        )),
        (QueryValue::Float { value, .. }, ScalarValue::Integer(reached)) => {
            ordered(compare_ratio_with_float(i128::from(reached), 1, *value))
        }
        (QueryValue::Float { value, .. }, ScalarValue::Float(reached)) => {
            ordered(reached.partial_cmp(value))
        }
        (QueryValue::Float { value, .. }, ScalarValue::Timestamp { epoch_ns }) => ordered(
            compare_ratio_with_float(i128::from(epoch_ns), NANOSECONDS_PER_MILLISECOND, *value),
        ),
        (
            QueryValue::Integer { text, .. } | QueryValue::Float { text, .. },
            ScalarValue::Text(reached),
        ) => is_equal.then(|| text_equals(reached, text.as_bytes(), is_case_sensitive)),

        (QueryValue::Boolean(value), ScalarValue::Boolean(reached)) => {
            is_equal.then_some(reached == *value)
        }
        (QueryValue::Boolean(value), ScalarValue::Text(reached)) => {
            let text: &[u8] = if *value { b"true" } else { b"false" };
            is_equal.then(|| text_equals(reached, text, is_case_sensitive))
        }

        (QueryValue::Null, ScalarValue::Null) => is_equal.then_some(true),
        (QueryValue::Null, ScalarValue::Text(reached)) => {
            is_equal.then(|| text_equals(reached, b"null", is_case_sensitive))
        }
        (QueryValue::Null, _) => is_equal.then_some(false),

        (QueryValue::Text(pattern), ScalarValue::Text(reached)) => {
            is_equal.then(|| pattern.matches(reached, is_case_sensitive))
        }

        (QueryValue::Timestamp { epoch_ns: value }, ScalarValue::Timestamp { epoch_ns }) => {
            ordered(Some(epoch_ns.cmp(value)))
        }
        (QueryValue::Timestamp { epoch_ns: value }, ScalarValue::Float(reached)) => ordered(
            compare_ratio_with_float(i128::from(*value), NANOSECONDS_PER_SECOND, reached)
                .map(Ordering::reverse),
        ),
        _ => None,
    }
}

/// Compares one reached value with an OR of equalities.
///
/// # Returns
///
/// `None` if `reached` is comparable with none of `values`, else whether it equals one of them.
pub fn equals_any(
    values: &[QueryValue],
    is_case_sensitive: bool,
    reached: ScalarValue<'_>,
) -> Option<bool> {
    let mut result = None;
    for value in values {
        match compare(value, CompareOperator::Equal, is_case_sensitive, reached) {
            Some(true) => return Some(true),
            Some(false) => result = Some(false),
            None => {}
        }
    }
    result
}

const NANOSECONDS_PER_MILLISECOND: u32 = 1_000_000;
const NANOSECONDS_PER_SECOND: u32 = 1_000_000_000;

/// # Returns
///
/// Whether `text` equals `expected`, folding ASCII case unless `is_case_sensitive`.
fn text_equals(text: &[u8], expected: &[u8], is_case_sensitive: bool) -> bool {
    if is_case_sensitive {
        text == expected
    } else {
        text.eq_ignore_ascii_case(expected)
    }
}

/// Orders the rational `numerator / denominator` relative to `float`, exactly.
///
/// # Returns
///
/// The ordering, or `None` if `float` is NaN.
fn compare_ratio_with_float(numerator: i128, denominator: u32, float: f64) -> Option<Ordering> {
    if float.is_nan() {
        return None;
    }
    if float.is_infinite() {
        return Some(if float > 0.0 {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    let bits = float.to_bits();
    let is_float_negative = 1 == bits >> 63;
    let exponent_bits = i32::try_from((bits >> 52) & 0x7ff).unwrap_or_default();
    let fraction = bits & ((1 << 52) - 1);
    let (mantissa, exponent) = if 0 == exponent_bits {
        (fraction, -1074)
    } else {
        (fraction | (1 << 52), exponent_bits - 1075)
    };

    // `float * denominator` is exactly `±scaled * 2^exponent`, with `scaled < 2^83`.
    let scaled = u128::from(mantissa) * u128::from(denominator);
    if 0 == scaled {
        return Some(numerator.cmp(&0));
    }
    if 0 == numerator {
        return Some(if is_float_negative {
            Ordering::Greater
        } else {
            Ordering::Less
        });
    }
    let magnitude_ordering =
        compare_with_scaled_power_of_two(numerator.unsigned_abs(), scaled, exponent);
    Some(match (numerator < 0, is_float_negative) {
        (false, true) => Ordering::Greater,
        (true, false) => Ordering::Less,
        (false, false) => magnitude_ordering,
        (true, true) => magnitude_ordering.reverse(),
    })
}

/// # Returns
///
/// The ordering of `left` relative to `scaled * 2^exponent`, for a non-zero `scaled`.
fn compare_with_scaled_power_of_two(left: u128, scaled: u128, exponent: i32) -> Ordering {
    let shift = exponent.unsigned_abs();
    if exponent >= 0 {
        if shift > scaled.leading_zeros() {
            return Ordering::Less;
        }
        return left.cmp(&(scaled << shift));
    }
    if shift >= u128::BITS {
        return if 0 == left {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    let floor = scaled >> shift;
    let has_remainder = 0 != scaled & ((1 << shift) - 1);
    match left.cmp(&floor) {
        Ordering::Equal if has_remainder => Ordering::Less,
        ordering => ordering,
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::compare;
    use super::compare_ratio_with_float;
    use super::equals_any;
    use crate::CompareOperator;
    use crate::PatternPiece;
    use crate::QueryValue;
    use crate::ScalarValue;
    use crate::SearchTypeSet;
    use crate::TextPattern;

    const OPERATORS: [CompareOperator; 5] = [
        CompareOperator::Equal,
        CompareOperator::Less,
        CompareOperator::LessOrEqual,
        CompareOperator::Greater,
        CompareOperator::GreaterOrEqual,
    ];

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
    /// The float value that `text` spells.
    ///
    /// # Panics
    ///
    /// Panics if `text` isn't a float literal.
    fn float(text: &str) -> QueryValue {
        QueryValue::Float {
            value: text.parse().expect("float literal should parse"),
            text: text.into(),
        }
    }

    /// # Returns
    ///
    /// A text value that matches `literal` literally.
    fn text(literal: &str) -> QueryValue {
        QueryValue::Text(TextPattern::new(vec![PatternPiece::Literal(
            literal.as_bytes().into(),
        )]))
    }

    /// # Returns
    ///
    /// Forwards [`compare`]'s return value for a case-sensitive equality of `reached` with `value`.
    fn equal(value: &QueryValue, reached: ScalarValue<'_>) -> Option<bool> {
        compare(value, CompareOperator::Equal, true, reached)
    }

    #[test]
    fn ratio_comparison_is_exact() {
        for (numerator, denominator, float, expected) in [
            (1, 1, 1.0, Some(Ordering::Equal)),
            (1, 1, 0.999_999_999_999_999_9, Some(Ordering::Greater)),
            (1, 3, 0.333_333_333_333_333_3, Some(Ordering::Greater)),
            (
                9_007_199_254_740_993,
                1,
                9_007_199_254_740_992.0,
                Some(Ordering::Greater),
            ),
            (
                i128::from(i64::MAX),
                1,
                9_223_372_036_854_775_807.0,
                Some(Ordering::Less),
            ),
            (-3, 1, -2.5, Some(Ordering::Less)),
            (-2, 1, -2.5, Some(Ordering::Greater)),
            (0, 1, -0.0, Some(Ordering::Equal)),
            (0, 1, 5e-324, Some(Ordering::Less)),
            (1, 1, 5e-324, Some(Ordering::Greater)),
            (-1, 1, 1e300, Some(Ordering::Less)),
            (1, 1, 1e300, Some(Ordering::Less)),
            (1, 1, -1e300, Some(Ordering::Greater)),
            (
                1_700_000_000_123_000_000,
                1_000_000,
                1_700_000_000_123.0,
                Some(Ordering::Equal),
            ),
            (
                1_700_000_000_123_400_000,
                1_000_000,
                1_700_000_000_123.0,
                Some(Ordering::Greater),
            ),
            (
                1_700_000_000_000_000_001,
                1_000_000_000,
                1_700_000_000.0,
                Some(Ordering::Greater),
            ),
            (5, 1, f64::INFINITY, Some(Ordering::Less)),
            (5, 1, f64::NEG_INFINITY, Some(Ordering::Greater)),
            (5, 1, f64::NAN, None),
        ] {
            assert_eq!(
                compare_ratio_with_float(numerator, denominator, float),
                expected,
                "{numerator}/{denominator} vs {float:e}"
            );
        }
    }

    #[test]
    fn numeric_literals_compare_exactly_across_types() {
        assert_eq!(equal(&integer(5), ScalarValue::Integer(5)), Some(true));
        assert_eq!(equal(&integer(5), ScalarValue::Float(5.0)), Some(true));
        assert_eq!(equal(&float("5.0"), ScalarValue::Integer(5)), Some(true));
        assert_eq!(equal(&float("5.5"), ScalarValue::Integer(5)), Some(false));
        assert_eq!(
            equal(
                &integer(9_007_199_254_740_993),
                ScalarValue::Float(9_007_199_254_740_992.0)
            ),
            Some(false)
        );
        assert_eq!(
            compare(
                &float("1e30"),
                CompareOperator::Greater,
                true,
                ScalarValue::Integer(2000)
            ),
            Some(false)
        );
        assert_eq!(
            compare(
                &float("-1e30"),
                CompareOperator::Greater,
                true,
                ScalarValue::Integer(2000)
            ),
            Some(true)
        );
        assert_eq!(
            compare(
                &float("2.5"),
                CompareOperator::LessOrEqual,
                true,
                ScalarValue::Integer(2)
            ),
            Some(true)
        );
    }

    #[test]
    fn numeric_literals_compare_with_timestamps_in_milliseconds() {
        let timestamp = ScalarValue::Timestamp {
            epoch_ns: 1_700_000_000_123_456_789,
        };
        assert_eq!(equal(&integer(1_700_000_000_123), timestamp), Some(false));
        assert_eq!(
            compare(
                &integer(1_700_000_000_123),
                CompareOperator::Greater,
                true,
                timestamp
            ),
            Some(true)
        );
        assert_eq!(
            compare(
                &integer(1_700_000_000_124),
                CompareOperator::Less,
                true,
                timestamp
            ),
            Some(true)
        );
        let exact_ms = ScalarValue::Timestamp {
            epoch_ns: 1_700_000_000_123_000_000,
        };
        assert_eq!(equal(&integer(1_700_000_000_123), exact_ms), Some(true));
        assert_eq!(equal(&float("1700000000123.0"), exact_ms), Some(true));
        assert_eq!(equal(&text("1700000000123"), exact_ms), None);
    }

    #[test]
    fn timestamp_literals_compare_in_nanoseconds_and_seconds() {
        let literal = QueryValue::Timestamp {
            epoch_ns: 1_500_000_000,
        };
        assert_eq!(
            equal(
                &literal,
                ScalarValue::Timestamp {
                    epoch_ns: 1_500_000_000
                }
            ),
            Some(true)
        );
        assert_eq!(
            equal(&literal, ScalarValue::Integer(1_500_000_000)),
            Some(true)
        );
        assert_eq!(equal(&literal, ScalarValue::Float(1.5)), Some(true));
        assert_eq!(
            compare(
                &literal,
                CompareOperator::Less,
                true,
                ScalarValue::Float(1.4)
            ),
            Some(true)
        );
        assert_eq!(equal(&literal, ScalarValue::Text(b"1500000000")), None);
        assert_eq!(equal(&literal, ScalarValue::Boolean(true)), None);
    }

    #[test]
    fn numeric_literals_match_their_exact_text() {
        assert_eq!(equal(&integer(5), ScalarValue::Text(b"5")), Some(true));
        assert_eq!(equal(&integer(5), ScalarValue::Text(b"5.0")), Some(false));
        assert_eq!(equal(&float("5.0"), ScalarValue::Text(b"5.0")), Some(true));
        assert_eq!(equal(&float("5.0"), ScalarValue::Text(b"5")), Some(false));
        assert_eq!(
            compare(
                &float("1e5"),
                CompareOperator::Equal,
                false,
                ScalarValue::Text(b"1E5")
            ),
            Some(true)
        );
        assert_eq!(
            compare(
                &integer(5),
                CompareOperator::Less,
                true,
                ScalarValue::Text(b"4")
            ),
            None
        );
    }

    #[test]
    fn booleans_nulls_and_texts_follow_the_coercion_table() {
        let boolean = QueryValue::Boolean(true);
        assert_eq!(equal(&boolean, ScalarValue::Boolean(true)), Some(true));
        assert_eq!(equal(&boolean, ScalarValue::Boolean(false)), Some(false));
        assert_eq!(equal(&boolean, ScalarValue::Text(b"true")), Some(true));
        assert_eq!(
            compare(
                &boolean,
                CompareOperator::Equal,
                false,
                ScalarValue::Text(b"TRUE")
            ),
            Some(true)
        );
        assert_eq!(equal(&boolean, ScalarValue::Integer(1)), None);
        assert_eq!(equal(&QueryValue::Null, ScalarValue::Null), Some(true));
        assert_eq!(
            equal(&QueryValue::Null, ScalarValue::Text(b"null")),
            Some(true)
        );
        assert_eq!(
            equal(&QueryValue::Null, ScalarValue::Text(b"none")),
            Some(false)
        );
        assert_eq!(
            equal(&QueryValue::Null, ScalarValue::Integer(0)),
            Some(false)
        );
        assert_eq!(
            equal(&QueryValue::Null, ScalarValue::Boolean(false)),
            Some(false)
        );
        assert_eq!(equal(&QueryValue::Null, ScalarValue::Object), None);
        assert_eq!(equal(&QueryValue::Null, ScalarValue::EmptyContainer), None);
        assert_eq!(equal(&text("a"), ScalarValue::Text(b"a")), Some(true));
        assert_eq!(equal(&text("5"), ScalarValue::Integer(5)), None);
        assert_eq!(equal(&text("true"), ScalarValue::Boolean(true)), None);
    }

    #[test]
    fn comparability_agrees_with_the_search_type_table() {
        let values = [
            integer(5),
            float("0.5"),
            QueryValue::Boolean(false),
            QueryValue::Null,
            text("x"),
            QueryValue::Timestamp { epoch_ns: 7 },
        ];
        let reached = [
            ScalarValue::Integer(1),
            ScalarValue::Float(1.0),
            ScalarValue::Text(b"x"),
            ScalarValue::Boolean(true),
            ScalarValue::Null,
            ScalarValue::Timestamp { epoch_ns: 7 },
            ScalarValue::Object,
            ScalarValue::EmptyContainer,
        ];
        for value in &values {
            for operator in OPERATORS {
                let search_types: SearchTypeSet = value.comparable_search_types(operator);
                for reached in reached {
                    let expected = reached
                        .search_type()
                        .is_some_and(|search_type| search_types.contains(search_type));
                    assert_eq!(
                        compare(value, operator, true, reached).is_some(),
                        expected,
                        "{value:?} {operator:?} {reached:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn in_sets_are_true_on_any_equal_member_and_unknown_when_nothing_compares() {
        let values = [integer(1), integer(2)];
        assert_eq!(
            equals_any(&values, true, ScalarValue::Integer(2)),
            Some(true)
        );
        assert_eq!(
            equals_any(&values, true, ScalarValue::Integer(3)),
            Some(false)
        );
        assert_eq!(equals_any(&values, true, ScalarValue::Boolean(true)), None);
        assert_eq!(equals_any(&[], true, ScalarValue::Integer(3)), None);
    }
}
