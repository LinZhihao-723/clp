//! The search semantics of design report §8.1 (`docs/design.md` Part 2), rule by rule, through the
//! reference evaluator over JSON records, with the in-scope divergences from C++ as positive cases.

use clp_s_query::ColumnPattern;
use clp_s_query::JsonRecord;
use clp_s_query::NormalExpr;
use clp_s_query::Query;
use clp_s_query::QueryOptions;
use clp_s_query::ScalarValue;
use clp_s_query::TimeBounds;
use clp_s_query::TruthValue;
use clp_s_query::TruthValue::False;
use clp_s_query::TruthValue::True;
use clp_s_query::TruthValue::Unknown;

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
/// The JSON value that `text` spells.
///
/// # Panics
///
/// Panics if `text` isn't valid JSON.
fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|error| panic!("{text:?}: {error}"))
}

/// Evaluates `kql` case-sensitively on the JSON record `record`, which has no timestamp.
///
/// # Returns
///
/// The truth value of the query on the record.
///
/// # Panics
///
/// Panics if `kql` or `record` is invalid.
fn truth(kql: &str, record: &str) -> TruthValue {
    let record = json(record);
    query(kql).evaluate(&JsonRecord::new(&record, None))
}

/// Evaluates `kql` case-insensitively on the JSON record `record`, which has no timestamp.
///
/// # Returns
///
/// The truth value of the query on the record.
///
/// # Panics
///
/// Panics if `kql` or `record` is invalid.
fn truth_ignoring_case(kql: &str, record: &str) -> TruthValue {
    let record = json(record);
    Query::from_kql(kql, &QueryOptions::default().with_case_sensitivity(false))
        .unwrap_or_else(|error| panic!("{kql:?}: {error}"))
        .evaluate(&JsonRecord::new(&record, None))
}

/// Evaluates `query` on the JSON record `record`, whose timestamp key is `ts`, stored as
/// `epoch_ns`.
///
/// # Returns
///
/// The truth value of the query on the record.
///
/// # Panics
///
/// Panics if `record` isn't valid JSON.
fn truth_with_timestamp(query: &Query, record: &str, epoch_ns: Option<i64>) -> TruthValue {
    let key = ColumnPattern::parse("ts").expect("the timestamp key should parse");
    let record = json(record);
    let timestamp = epoch_ns.map(|epoch_ns| ScalarValue::Timestamp { epoch_ns });
    query.evaluate(&JsonRecord::new(&record, timestamp).with_timestamp_key(&key))
}

/// Asserts that `kql` evaluates to the expected truth value on each JSON record of `cases`.
///
/// # Panics
///
/// Panics if a case evaluates to another truth value, or if `kql` or a record is invalid.
fn assert_truths(kql: &str, cases: &[(&str, TruthValue)]) {
    for (record, expected) in cases {
        assert_eq!(truth(kql, record), *expected, "{kql:?} on {record}");
    }
}

#[test]
fn exists_is_true_or_false_and_never_unknown() {
    assert_truths(
        "a: *",
        &[(r#"{"a": 1}"#, True), (r#"{"b": 1}"#, False), ("{}", False)],
    );
    assert_truths("NOT a: *", &[(r#"{"a": 1}"#, False), (r#"{"b": 1}"#, True)]);
}

#[test]
fn objects_and_empty_containers_satisfy_exists_dv_19() {
    assert_truths(
        "o: *",
        &[
            (r#"{"o": {"x": 1}}"#, True),
            (r#"{"o": {}}"#, True),
            (r#"{"o": []}"#, True),
            (r#"{"o": [[]]}"#, True),
            (r#"{"o": null}"#, True),
        ],
    );
}

#[test]
fn nexists_matches_every_record_lacking_the_path_dv_07() {
    assert_truths("NOT zzz: *", &[(r#"{"a": 1}"#, True), ("{}", True)]);
}

#[test]
fn a_comparison_is_unknown_without_a_comparable_value() {
    assert_truths(
        "a: 5",
        &[
            (r#"{"a": 5}"#, True),
            (r#"{"a": 6}"#, False),
            (r#"{"a": true}"#, Unknown),
            (r#"{"a": null}"#, Unknown),
            (r#"{"a": {"x": 5}}"#, Unknown),
            (r#"{"b": 5}"#, Unknown),
        ],
    );
    assert_truths(
        "NOT a: 5",
        &[(r#"{"a": true}"#, Unknown), (r#"{"a": 6}"#, True)],
    );
}

#[test]
fn a_comparison_is_existential_over_the_reached_values() {
    assert_truths(
        "a: 2",
        &[(r#"{"a": [1, 2]}"#, True), (r#"{"a": [1, 3]}"#, False)],
    );
    assert_truths(
        "NOT a: 2",
        &[(r#"{"a": [1, 2]}"#, False), (r#"{"a": [1, 3]}"#, True)],
    );
}

#[test]
fn connectives_follow_kleene_and_only_true_matches() {
    assert_truths(
        "a: 1 OR b: 1",
        &[
            (r#"{"a": 1}"#, True),
            (r#"{"a": 2}"#, Unknown),
            (r#"{"a": 2, "b": 2}"#, False),
        ],
    );
    assert_truths(
        "a: 1 AND b: 1",
        &[
            (r#"{"a": 1}"#, Unknown),
            (r#"{"a": 2}"#, False),
            (r#"{"a": 1, "b": 1}"#, True),
        ],
    );
    assert_truths(
        "NOT (a: 1 AND b: 1)",
        &[(r#"{"a": 1}"#, Unknown), (r#"{"a": 2}"#, True)],
    );
}

#[test]
fn column_patterns_are_quantifiers_not_disjunctions_dv_08() {
    let kql = "NOT a.*.b: 1";
    assert_truths(
        kql,
        &[
            (r#"{"a": {"x": {"b": 2}, "y": {"b": 1}}}"#, False),
            (r#"{"a": {"x": {"b": 2}, "y": {"b": 3}}}"#, True),
            (r#"{"a": {"x": {"c": 2}}}"#, Unknown),
        ],
    );
    assert_truths(
        "NOT a.*.b: *",
        &[(r#"{"a": {"x": {"b": 2}, "y": {}}}"#, False)],
    );
}

#[test]
fn literal_paths_and_wildcards_descend_into_arrays_dv_36() {
    assert_truths("a.b: 2", &[(r#"{"a": [{"b": 1}, {"b": 2}]}"#, True)]);
    assert_truths("a.*.b: 1", &[(r#"{"a": [{"x": {"b": 1}}]}"#, True)]);
    assert_truths(
        "*.b: 1",
        &[
            (r#"{"l": [{"b": 1}]}"#, True),
            (r#"{"l": [[{"b": 1}]]}"#, True),
        ],
    );
    assert_truths("a: 3", &[(r#"{"a": [1, [2, [3]]]}"#, True)]);
}

#[test]
fn bare_values_reach_every_leaf() {
    assert_truths(
        "foo",
        &[
            (r#"{"x": {"y": ["foo"]}}"#, True),
            (r#"{"x": "foo"}"#, True),
            (r#"{"x": "bar"}"#, False),
            (r#"{"x": 1}"#, Unknown),
        ],
    );
    assert_truths(
        "5",
        &[(r#"{"x": [{"y": 5}]}"#, True), (r#"{"x": "5"}"#, True)],
    );
}

#[test]
fn not_forms_are_unknown_on_empty_value_sets_dv_13() {
    assert_truths(
        "NOT a.x: 1",
        &[
            (r#"{"a": [1, 2]}"#, Unknown),
            (r#"{"a": [{"x": 2}]}"#, True),
        ],
    );
    assert_truths(
        "NOT a.x: *",
        &[(r#"{"a": [1, 2]}"#, True), (r#"{"a": [{"x": 2}]}"#, False)],
    );
}

#[test]
fn integer_literals_follow_the_coercion_table() {
    assert_truths(
        "a: 5",
        &[
            (r#"{"a": 5.0}"#, True),
            (r#"{"a": "5"}"#, True),
            (r#"{"a": "5.0"}"#, False),
            (r#"{"a": "+5"}"#, False),
            (r#"{"a": [4, 5]}"#, True),
        ],
    );
    assert_truths("a: +5", &[(r#"{"a": "+5"}"#, True), (r#"{"a": 5}"#, True)]);
    assert_truths(
        "a > 5",
        &[
            (r#"{"a": 6}"#, True),
            (r#"{"a": 5.5}"#, True),
            (r#"{"a": 5}"#, False),
            (r#"{"a": "6"}"#, Unknown),
            (r#"{"a": true}"#, Unknown),
        ],
    );
    assert_truths(
        "a: 9007199254740993",
        &[
            (r#"{"a": 9007199254740993}"#, True),
            (r#"{"a": 9007199254740992.0}"#, False),
        ],
    );
}

#[test]
fn float_literals_compare_exactly_dv_14() {
    assert_truths(
        "a: 5.0",
        &[
            (r#"{"a": 5}"#, True),
            (r#"{"a": "5.0"}"#, True),
            (r#"{"a": "5"}"#, False),
        ],
    );
    assert_truths("a: 5.5", &[(r#"{"a": 5}"#, False)]);
    assert_truths(
        "a > 1e30",
        &[(r#"{"a": 2000}"#, False), (r#"{"a": 1e31}"#, True)],
    );
    assert_truths("a < -1e30", &[(r#"{"a": 2000}"#, False)]);
    assert_truths(
        "a >= 9223372036854775807.0",
        &[(r#"{"a": 9223372036854775807}"#, False)],
    );
}

#[test]
fn boolean_literals_follow_the_coercion_table() {
    assert_truths(
        "a: true",
        &[
            (r#"{"a": true}"#, True),
            (r#"{"a": false}"#, False),
            (r#"{"a": "true"}"#, True),
            (r#"{"a": "yes"}"#, False),
            (r#"{"a": 1}"#, Unknown),
        ],
    );
    assert_eq!(*query("a > true").root(), NormalExpr::MatchNone);
}

#[test]
fn null_literals_follow_the_coercion_table_dv_10_dv_12() {
    assert_truths(
        "a: null",
        &[
            (r#"{"a": null}"#, True),
            (r#"{"a": "null"}"#, True),
            (r#"{"a": 0}"#, False),
            (r#"{"a": false}"#, False),
            (r#"{"a": "x"}"#, False),
            (r#"{"a": [null]}"#, True),
            (r#"{"a": [1, 2]}"#, False),
            (r#"{"a": {"x": 1}}"#, Unknown),
            (r#"{"a": {}}"#, Unknown),
        ],
    );
    assert_truths(
        "NOT a: null",
        &[
            (r#"{"a": "null"}"#, False),
            (r#"{"a": null}"#, False),
            (r#"{"a": 5}"#, True),
            (r#"{"a": [1]}"#, True),
            (r#"{"a": {"x": 1}}"#, Unknown),
        ],
    );
    assert_eq!(*query("a < null").root(), NormalExpr::MatchNone);
}

#[test]
fn text_patterns_are_whole_value_byte_globs() {
    assert_truths(
        "a: \"he*o\"",
        &[
            (r#"{"a": "hello"}"#, True),
            (r#"{"a": "hello!"}"#, False),
            (r#"{"a": "heo"}"#, True),
        ],
    );
    assert_truths(
        "a: h?llo",
        &[(r#"{"a": "hello"}"#, True), (r#"{"a": "hllo"}"#, False)],
    );
    assert_truths("a: \"caf??\"", &[("{\"a\": \"caf\u{e9}\"}", True)]);
    assert_truths(
        "a: 5*",
        &[(r#"{"a": 55}"#, Unknown), (r#"{"a": "55"}"#, True)],
    );
    assert_truths(
        "a: \\*",
        &[(r#"{"a": "*"}"#, True), (r#"{"a": "x"}"#, False)],
    );
    assert_eq!(*query("a > foo").root(), NormalExpr::MatchNone);
}

#[test]
fn case_insensitive_matching_folds_ascii_only() {
    assert_eq!(truth_ignoring_case("a: ERROR", r#"{"a": "error"}"#), True);
    assert_eq!(truth("a: ERROR", r#"{"a": "error"}"#), False);
    assert_eq!(
        truth_ignoring_case("a: \"*FAIL*\"", r#"{"a": "it failed"}"#),
        True
    );
    assert_eq!(
        truth_ignoring_case("a: \"\u{c9}\"", "{\"a\": \"\u{e9}\"}"),
        False
    );
    assert_eq!(truth_ignoring_case("a: true", r#"{"a": "TRUE"}"#), True);
    assert_eq!(truth_ignoring_case("a: null", r#"{"a": "NULL"}"#), True);
}

#[test]
fn a_text_without_a_space_is_false_on_clp_strings() {
    assert_truths(
        "NOT msg: hello",
        &[
            (r#"{"msg": "hello world"}"#, True),
            (r#"{"msg": "hello"}"#, False),
        ],
    );
    assert_truths("msg: hello*", &[(r#"{"msg": "hello world"}"#, True)]);
    assert_truths("msg: \"hello world\"", &[(r#"{"msg": "hello"}"#, False)]);
}

#[test]
fn timestamp_literals_compare_in_nanoseconds_and_seconds() {
    let kql = r#"a: timestamp("1500000000", "\N")"#;
    assert_truths(
        kql,
        &[
            (r#"{"a": 1500000000}"#, True),
            (r#"{"a": 1.5}"#, True),
            (r#"{"a": "1500000000"}"#, Unknown),
        ],
    );
    let parsed = query(r#"ts > timestamp("1700000000000", "\L")"#);
    assert_eq!(
        truth_with_timestamp(&parsed, r#"{"ts": 1}"#, Some(1_700_000_000_000_000_001)),
        True
    );
    assert_eq!(
        truth_with_timestamp(&parsed, r#"{"ts": 1}"#, Some(1_700_000_000_000_000_000)),
        False
    );
}

#[test]
fn numeric_literals_compare_with_timestamps_in_milliseconds_dv_15() {
    let stored_ns = 1_700_000_004_123_456_789;
    let record = r#"{"ts": 1700000004123456789}"#;
    assert_eq!(
        truth_with_timestamp(&query("ts > 1700000004123"), record, Some(stored_ns)),
        True
    );
    assert_eq!(
        truth_with_timestamp(&query("ts < 1700000004124"), record, Some(stored_ns)),
        True
    );
    assert_eq!(
        truth_with_timestamp(&query("ts: 1700000004123456789"), record, Some(stored_ns)),
        False
    );
    let exact = r#"{"ts": 1700000004123}"#;
    assert_eq!(
        truth_with_timestamp(
            &query("ts: 1700000004123"),
            exact,
            Some(1_700_000_004_123_000_000)
        ),
        True
    );
    assert_eq!(
        truth_with_timestamp(
            &query("ts: \"1700000004123\""),
            exact,
            Some(1_700_000_004_123_000_000)
        ),
        True
    );
    assert_eq!(
        truth_with_timestamp(&query("ts: \"2023*\""), r#"{"ts": "2023-11-14"}"#, Some(1)),
        Unknown
    );
}

#[test]
fn star_matches_every_record_and_not_star_none_dv_09() {
    for kql in ["*", "*:*", "\"*\"", "*.*: *"] {
        assert_eq!(*query(kql).root(), NormalExpr::MatchAll, "{kql}");
        assert_eq!(truth(kql, "{}"), True, "{kql}");
    }
    for kql in ["NOT *", "NOT *:*"] {
        assert_eq!(*query(kql).root(), NormalExpr::MatchNone, "{kql}");
    }
    assert_truths(
        "*: null",
        &[
            (r#"{"a": 1}"#, False),
            (r#"{"a": null}"#, True),
            ("{}", Unknown),
        ],
    );
    assert_truths(
        "NOT *: null",
        &[(r#"{"a": 1}"#, True), (r#"{"a": null}"#, False)],
    );
}

#[test]
fn and_and_or_keep_equal_precedence() {
    assert_truths(
        "a: 1 OR a: 2 AND b: 3",
        &[
            (r#"{"a": 1}"#, Unknown),
            (r#"{"a": 1, "b": 3}"#, True),
            (r#"{"a": 1, "b": 4}"#, False),
        ],
    );
    assert_truths("NOT a: 1 OR b: 1", &[(r#"{"a": 2}"#, True)]);
}

#[test]
fn negated_bare_values_are_domain_restricted_dv_05() {
    assert_truths(
        "NOT ERROR",
        &[
            (r#"{"level": "INFO", "n": 5}"#, True),
            (r#"{"level": "ERROR"}"#, False),
            (r#"{"n": 5}"#, Unknown),
        ],
    );
}

#[test]
fn empty_keys_are_never_transparent_dv_16_dv_38() {
    assert_truths(
        "a.b: 1",
        &[
            (r#"{"a": {"": {"b": 1}}}"#, Unknown),
            (r#"{"a": {"b": 1}}"#, True),
        ],
    );
    assert_truths("a.*.b: 1", &[(r#"{"a": {"": {"b": 1}}}"#, True)]);
    assert_truths("*.b: 1", &[(r#"{"": {"b": 1}}"#, True)]);
}

#[test]
fn wildcards_bind_every_matching_path_dv_37_dv_39() {
    assert_truths(
        "a.*: 1",
        &[
            (r#"{"a": {"x": 2, "y": 1}}"#, True),
            (r#"{"a": {"x": 2, "y": [3, {"z": 1}]}}"#, True),
        ],
    );
    assert_truths(
        "a.*.c: 1",
        &[(r#"{"a": [{"b": {"c": 2}}, {"d": [{"c": 1}]}]}"#, True)],
    );
}

#[test]
fn array_elements_are_typed_and_ranges_apply_dv_11_dv_12() {
    assert_truths(
        "a > 5",
        &[
            (r#"{"a": [1, 10]}"#, True),
            (r#"{"a": [1, 2]}"#, False),
            (r#"{"a": ["x", "y"]}"#, Unknown),
            (r#"{"a": [1.5, 6.5]}"#, True),
        ],
    );
    assert_truths(
        "NOT a: 2",
        &[
            (r#"{"a": [1, 2]}"#, False),
            (r#"{"a": ["x"]}"#, True),
            (r#"{"a": [null]}"#, Unknown),
        ],
    );
    assert_truths(
        "NOT a: x",
        &[(r#"{"a": [1, 2]}"#, Unknown), (r#"{"a": ["y"]}"#, True)],
    );
}

#[test]
fn time_bounds_are_inclusive_and_unknown_without_a_timestamp() {
    let bounded = query("*").with_time_bounds(TimeBounds::from_epoch_millis(Some(10), Some(20)));
    for (epoch_ns, expected) in [
        (Some(10_000_000), True),
        (Some(20_000_000), True),
        (Some(9_999_999), False),
        (Some(20_000_001), False),
        (None, Unknown),
    ] {
        assert_eq!(
            truth_with_timestamp(&bounded, "{}", epoch_ns),
            expected,
            "{epoch_ns:?}"
        );
    }
    let not_exists =
        query("NOT ts: *").with_time_bounds(TimeBounds::from_epoch_millis(Some(0), None));
    assert_eq!(truth_with_timestamp(&not_exists, "{}", None), Unknown);
    assert_eq!(
        truth_with_timestamp(&not_exists, r#"{"ts": 5}"#, Some(5_000_000_000)),
        False
    );
}

#[test]
fn time_bounds_saturate_dv_42() {
    let bounded = query("*").with_time_bounds(TimeBounds::from_epoch_millis(
        Some(i64::MIN),
        Some(i64::MAX),
    ));
    assert_eq!(truth_with_timestamp(&bounded, "{}", Some(i64::MIN)), True);
    assert_eq!(truth_with_timestamp(&bounded, "{}", Some(i64::MAX)), True);
}

#[test]
fn logically_false_queries_fold_without_an_error_dv_20() {
    for kql in ["a > foo", "NOT a > foo", "a > foo AND b: 1", "a: ()"] {
        assert_eq!(*query(kql).root(), NormalExpr::MatchNone, "{kql}");
    }
    assert_eq!(truth("b: 1 OR a > foo", r#"{"b": 1}"#), True);
}

#[test]
fn range_index_fields_live_in_the_dollar_namespace() {
    let fields = json(r#"{"_filename": "search.jsonl"}"#);
    let record = json(r#"{"_filename": "other"}"#);
    let source = JsonRecord::new(&record, None).with_range_index_fields(&fields);
    assert_eq!(
        query(r#"$_filename: "search.jsonl""#).evaluate(&source),
        True
    );
    assert_eq!(
        query(r#"$_filename: "other.jsonl""#).evaluate(&source),
        False
    );
    assert_eq!(query(r#"_filename: "other""#).evaluate(&source), True);
    assert_eq!(query(r#"@_filename: "other""#).evaluate(&source), Unknown);
}

#[test]
fn in_set_folding_preserves_unknown() {
    for (kql, record, expected) in [
        ("a: (1 foo)", r#"{"a": 5}"#, Unknown),
        ("NOT a: (1 foo)", r#"{"a": 5}"#, Unknown),
        ("a: (1 2)", r#"{"a": 5}"#, False),
        ("NOT a: (1 2)", r#"{"a": 5}"#, True),
        ("NOT a: (1 2)", r#"{"a": true}"#, Unknown),
        ("a: (NOT 1 2)", r#"{"a": 2}"#, False),
        ("a: (NOT 1 2)", r#"{"a": 3}"#, True),
        ("a: 1 OR a: 2.5", r#"{"a": "2.5"}"#, True),
    ] {
        assert_eq!(truth(kql, record), expected, "{kql:?} on {record}");
    }
}
