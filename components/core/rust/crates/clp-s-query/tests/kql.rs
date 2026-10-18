//! KQL parser tests: a port of the C++ `clp_s/tests/test-kql.cpp` cases plus the grammar rules of
//! `Kql.g4` (precedence, lists, nested queries, literal typing and limits).

use clp_s_query::ColumnPattern;
use clp_s_query::ColumnRef;
use clp_s_query::CompareOperator;
use clp_s_query::LogicalExpr;
use clp_s_query::Namespace;
use clp_s_query::PathToken;
use clp_s_query::PatternPiece;
use clp_s_query::Predicate;
use clp_s_query::PredicateTest;
use clp_s_query::QueryError;
use clp_s_query::QueryLimits;
use clp_s_query::QueryOptions;
use clp_s_query::QueryValue;
use clp_s_query::TextPattern;

/// Parses `text` as KQL with the default options.
///
/// # Returns
///
/// The parsed expression.
///
/// # Panics
///
/// Panics if `text` doesn't parse.
fn parse(text: &str) -> LogicalExpr {
    LogicalExpr::parse_kql(text, &QueryOptions::default())
        .unwrap_or_else(|error| panic!("{text:?} should parse: {error}"))
}

/// Parses `text` as KQL with the default options, expecting a failure.
///
/// # Returns
///
/// The parse error.
///
/// # Panics
///
/// Panics if `text` parses.
fn parse_error(text: &str) -> QueryError {
    match LogicalExpr::parse_kql(text, &QueryOptions::default()) {
        Ok(expr) => panic!("{text:?} should not parse, got {expr:?}"),
        Err(error) => error,
    }
}

/// # Returns
///
/// A key path token named `text`.
fn key(text: &str) -> PathToken {
    PathToken::Key(text.as_bytes().into())
}

/// # Returns
///
/// A default-namespace column made of `tokens`.
fn path(tokens: Vec<PathToken>) -> ColumnRef {
    ColumnRef::Path(ColumnPattern {
        namespace: Namespace::Default,
        tokens: tokens.into_boxed_slice(),
    })
}

/// # Returns
///
/// A text value that matches `text` literally.
fn literal_text(text: &str) -> QueryValue {
    QueryValue::Text(TextPattern::new(vec![PatternPiece::Literal(
        text.as_bytes().into(),
    )]))
}

/// # Returns
///
/// A case-sensitive comparison of `column` with `value` under `operator`.
const fn compare(column: ColumnRef, operator: CompareOperator, value: QueryValue) -> LogicalExpr {
    LogicalExpr::Predicate(Predicate {
        column,
        test: PredicateTest::Compare {
            operator,
            value,
            is_case_sensitive: true,
        },
    })
}

/// # Returns
///
/// A case-sensitive equality comparison of `column` with `value`.
const fn equal(column: ColumnRef, value: QueryValue) -> LogicalExpr {
    compare(column, CompareOperator::Equal, value)
}

/// # Returns
///
/// An existence test of `column`.
const fn exists(column: ColumnRef) -> LogicalExpr {
    LogicalExpr::Predicate(Predicate {
        column,
        test: PredicateTest::Exists,
    })
}

/// # Returns
///
/// The negation of `expr`.
fn not(expr: LogicalExpr) -> LogicalExpr {
    LogicalExpr::Not(Box::new(expr))
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

#[test]
fn pure_wildcard_key_queries() {
    for query in [
        "value",
        "  value ",
        "\"value\"",
        " \"value\" ",
        "*:value",
        " * : value ",
        "*:\"value\"",
        " *:   \"value\" ",
        "\"*\":value",
        " \"*\" :value",
        "\"*\":\"value\"",
        "   \"*\":  \"value\" ",
    ] {
        assert_eq!(
            parse(query),
            equal(ColumnRef::AnyLeaf, literal_text("value")),
            "{query:?}"
        );
    }
}

#[test]
fn basic_filter() {
    for query in ["key:value", " key   : value "] {
        assert_eq!(
            parse(query),
            equal(path(vec![key("key")]), literal_text("value")),
            "{query:?}"
        );
    }
}

#[test]
fn basic_not_filter() {
    for query in ["not key:value", " not key  : value "] {
        assert_eq!(
            parse(query),
            not(equal(path(vec![key("key")]), literal_text("value"))),
            "{query:?}"
        );
    }
}

#[test]
fn incorrect_not_filter() {
    for query in ["NOT :", "NOT key: ", "NOT  : value"] {
        assert!(
            matches!(parse_error(query), QueryError::Syntax { .. }),
            "{query:?}"
        );
    }
}

#[test]
fn basic_and_and_or_expressions() {
    for (connective, build) in [
        (
            "and",
            LogicalExpr::And as fn(Vec<LogicalExpr>) -> LogicalExpr,
        ),
        ("or", LogicalExpr::Or),
    ] {
        for spelling in [connective.to_owned(), connective.to_uppercase(), {
            let mut mixed = connective.to_uppercase();
            mixed.replace_range(..1, &connective[..1]);
            mixed
        }] {
            for query in [
                format!("key1:value1 {spelling} key2:value2"),
                format!("key1  : value1 {spelling}  key2  : value2"),
            ] {
                assert_eq!(
                    parse(&query),
                    build(vec![
                        equal(path(vec![key("key1")]), literal_text("value1")),
                        equal(path(vec![key("key2")]), literal_text("value2")),
                    ]),
                    "{query:?}"
                );
            }
        }
    }
}

#[test]
fn incorrect_and_and_or_filters() {
    for query in [
        "a : a AND b :",
        " : a AND b :",
        ": a AND b :b",
        " AND b :b",
        "a: a AND",
        "a : a OR b :",
        " : a OR b :",
        ": a OR b :b",
        " OR b :b",
        "a: a OR",
    ] {
        assert!(
            matches!(parse_error(query), QueryError::Syntax { .. }),
            "{query:?}"
        );
    }
}

#[test]
fn escape_sequences_in_column_name() {
    for query in [
        "a\\.b.c: *",
        "\"a\\.b.c\": *",
        "a\\.b: {c: *}",
        "\"a\\.b\": {\"c\": *}",
    ] {
        assert_eq!(
            parse(query),
            exists(path(vec![key("a.b"), key("c")])),
            "{query:?}"
        );
    }
}

#[test]
fn illegal_escape_sequences_in_column_name() {
    for query in [
        "\"a\\\":*",
        "a\\ :*",
        "\"a\\\" :*",
        "a.:*",
        "\"a.\":*",
        "a. :*",
        "\"a.\" :*",
    ] {
        assert!(
            matches!(
                parse_error(query),
                QueryError::InvalidColumn { .. } | QueryError::InvalidEscape { .. }
            ),
            "{query:?}"
        );
    }
}

#[test]
fn empty_token_in_column_name() {
    for query in [".a:*", "a.:*", "a..c:*", "a.b.:*"] {
        assert!(
            matches!(parse_error(query), QueryError::InvalidColumn { .. }),
            "{query:?}"
        );
    }
}

#[test]
fn escape_sequences_in_value() {
    let bytes = |text: &str| PatternPiece::Literal(text.as_bytes().into());
    for (raw, pieces) in [
        ("\\\\", vec![bytes("\\")]),
        ("\\??", vec![bytes("?"), PatternPiece::AnyByte]),
        ("\\**", vec![bytes("*"), PatternPiece::AnyRun]),
        ("\\u9999", vec![bytes("\u{9999}")]),
        ("\\r\\n\\t\\b\\f", vec![bytes("\r\n\t\u{8}\u{c}")]),
        ("\\\"", vec![bytes("\"")]),
        ("\\{\\}\\(\\)\\<\\>", vec![bytes("{}()<>")]),
        ("\\u003F", vec![bytes("?")]),
        ("\\u002A", vec![bytes("*")]),
        ("\\u005C", vec![bytes("\\")]),
    ] {
        let query = format!("*: \"{raw}\"");
        assert_eq!(
            parse(&query),
            equal(
                ColumnRef::AnyLeaf,
                QueryValue::Text(TextPattern::new(pieces))
            ),
            "{query:?}"
        );
    }
}

#[test]
fn namespaced_column() {
    for (sigil, namespace) in [
        ("@", Namespace::AutoGenerated),
        ("$", Namespace::RangeIndex),
        ("!", Namespace::Reserved(b'!')),
        ("#", Namespace::Reserved(b'#')),
    ] {
        for query in [
            format!("{sigil}column : *"),
            format!("\"{sigil}column\": *"),
        ] {
            let pattern = ColumnPattern {
                namespace,
                tokens: Box::new([key("column")]),
            };
            assert_eq!(
                parse(&query),
                exists(ColumnRef::from_pattern(pattern)),
                "{query:?}"
            );
        }
    }
}

#[test]
fn escaped_namespace_in_column() {
    for sigil in ["@", "$", "!", "#"] {
        for query in [
            format!("\\{sigil}column : *"),
            format!("\"\\{sigil}column\": *"),
        ] {
            assert_eq!(
                parse(&query),
                exists(path(vec![key(&format!("{sigil}column"))])),
                "{query:?}"
            );
        }
    }
}

#[test]
fn timestamp_expressions_are_parsed_correctly() {
    for (query, operator) in [
        (r#"* : timestamp("1", "\N")"#, CompareOperator::Equal),
        (r#"* < timestamp("1", "\N")"#, CompareOperator::Less),
    ] {
        assert_eq!(
            parse(query),
            compare(
                ColumnRef::AnyLeaf,
                operator,
                QueryValue::Timestamp { epoch_ns: 1 }
            ),
            "{query:?}"
        );
    }
}

#[test]
fn date_time_timestamp_expressions_are_not_supported_yet() {
    for query in [
        r#"* : timestamp("1970-01-01 00:00:00.000000001")"#,
        r#"* < timestamp("1970-01-01 00:00:00.000000001")"#,
    ] {
        assert!(
            matches!(parse_error(query), QueryError::InvalidTimestamp { .. }),
            "{query:?}"
        );
    }
}

#[test]
fn invalid_timestamp_expressions_are_rejected() {
    for query in [
        r"a: timestamp(",
        r"a: timestamp()",
        r"a: timestamp(,)",
        r#"a: timestamp("1","#,
        r#"a: timestamp("1)"#,
        r"a: timestamp(1)",
        r#"a: timestamp("a")"#,
        r#"a: timestamp("12345", "\Y-\m-\d")"#,
        r#"a: timestamp("12345", "\N))"#,
    ] {
        assert!(
            matches!(
                parse_error(query),
                QueryError::Syntax { .. } | QueryError::InvalidTimestamp { .. }
            ),
            "{query:?}"
        );
    }
}

#[test]
fn and_and_or_have_equal_precedence_and_associate_left() {
    let a = || equal(path(vec![key("a")]), integer(1));
    let b = || equal(path(vec![key("b")]), integer(2));
    let c = || equal(path(vec![key("c")]), integer(3));
    assert_eq!(
        parse("a: 1 OR b: 2 AND c: 3"),
        LogicalExpr::And(vec![LogicalExpr::Or(vec![a(), b()]), c()])
    );
    assert_eq!(
        parse("a: 1 AND b: 2 OR c: 3"),
        LogicalExpr::Or(vec![LogicalExpr::And(vec![a(), b()]), c()])
    );
    assert_eq!(
        parse("a: 1 OR (b: 2 AND c: 3)"),
        LogicalExpr::Or(vec![a(), LogicalExpr::And(vec![b(), c()])])
    );
    assert_eq!(
        parse("a: 1 AND b: 2 AND c: 3"),
        LogicalExpr::And(vec![a(), b(), c()])
    );
    assert_eq!(
        parse("(a: 1 AND b: 2) AND c: 3"),
        LogicalExpr::And(vec![LogicalExpr::And(vec![a(), b()]), c()])
    );
}

#[test]
fn not_binds_tighter_than_and_and_or() {
    let a = || equal(path(vec![key("a")]), integer(1));
    let b = || equal(path(vec![key("b")]), integer(2));
    assert_eq!(
        parse("NOT a: 1 AND b: 2"),
        LogicalExpr::And(vec![not(a()), b()])
    );
    assert_eq!(
        parse("NOT (a: 1 AND b: 2)"),
        not(LogicalExpr::And(vec![a(), b()]))
    );
    assert_eq!(parse("NOT NOT a: 1"), not(not(a())));
    assert_eq!(
        parse("a: 1 OR NOT b: 2"),
        LogicalExpr::Or(vec![a(), not(b())])
    );
}

#[test]
fn lists_expand_into_equalities_on_the_column() {
    let column = || path(vec![key("a")]);
    assert_eq!(
        parse("a: (1 foo)"),
        LogicalExpr::Or(vec![
            equal(column(), integer(1)),
            equal(column(), literal_text("foo"))
        ])
    );
    assert_eq!(
        parse("a: (OR 1 2)"),
        LogicalExpr::Or(vec![
            equal(column(), integer(1)),
            equal(column(), integer(2))
        ])
    );
    assert_eq!(
        parse("a: (and 1 2)"),
        LogicalExpr::And(vec![
            equal(column(), integer(1)),
            equal(column(), integer(2))
        ])
    );
    assert_eq!(
        parse("a: (NOT 1 *)"),
        LogicalExpr::And(vec![
            not(equal(column(), integer(1))),
            not(exists(column()))
        ])
    );
    assert_eq!(parse("a: ()"), LogicalExpr::Or(vec![]));
    assert_eq!(parse("a: (AND)"), LogicalExpr::And(vec![]));
    for query in ["a: (b: c)", "a: (1 AND 2)", "a > (1 2)", "a: (1", "(a b)"] {
        assert!(
            matches!(parse_error(query), QueryError::Syntax { .. }),
            "{query:?}"
        );
    }
}

#[test]
fn nested_queries_prefix_every_column() -> anyhow::Result<()> {
    assert_eq!(
        parse("req: {user: {id: 7}}"),
        equal(path(vec![key("req"), key("user"), key("id")]), integer(7))
    );
    assert_eq!(
        parse("a: {b: 1 AND NOT c > 2}"),
        LogicalExpr::And(vec![
            equal(path(vec![key("a"), key("b")]), integer(1)),
            not(compare(
                path(vec![key("a"), key("c")]),
                CompareOperator::Greater,
                integer(2)
            )),
        ])
    );
    assert_eq!(
        parse("a: {foo}"),
        equal(
            path(vec![key("a"), PathToken::AnyDepth]),
            literal_text("foo")
        )
    );
    assert_eq!(
        parse("*: {foo}"),
        equal(ColumnRef::AnyLeaf, literal_text("foo"))
    );
    assert_eq!(
        parse("a: {b: (1 2)}"),
        LogicalExpr::Or(vec![
            equal(path(vec![key("a"), key("b")]), integer(1)),
            equal(path(vec![key("a"), key("b")]), integer(2)),
        ])
    );
    assert_eq!(
        parse("@a: {b: 1}"),
        equal(ColumnRef::Path(ColumnPattern::parse("@a.b")?), integer(1))
    );
    assert_eq!(
        parse("$a: {b: 1}"),
        equal(
            ColumnRef::RangeIndexField(ColumnPattern::parse("$a.b")?),
            integer(1)
        )
    );
    for query in ["a: {@b: 1}", "a: {b: {$c: 1}}"] {
        assert!(
            matches!(parse_error(query), QueryError::NamespaceInNestedQuery),
            "{query:?}"
        );
    }
    for query in ["a: {}", "a: {b: 1", "a: {b: 1))", "{b: 1}"] {
        assert!(
            matches!(parse_error(query), QueryError::Syntax { .. }),
            "{query:?}"
        );
    }
    Ok(())
}

#[test]
fn quoting_does_not_change_literal_typing() {
    let column = || path(vec![key("n")]);
    assert_eq!(parse("n: \"5\""), equal(column(), integer(5)));
    assert_eq!(
        parse("n: \"true\""),
        equal(column(), QueryValue::Boolean(true))
    );
    assert_eq!(parse("n: \"null\""), equal(column(), QueryValue::Null));
    assert_eq!(parse("n: \" 5\""), equal(column(), literal_text(" 5")));
    assert_eq!(
        parse("n: 3.0"),
        equal(
            column(),
            QueryValue::Float {
                value: 3.0,
                text: "3.0".into()
            }
        )
    );
    for text in ["0x10", "inf", "nan", "1e400"] {
        assert_eq!(
            parse(&format!("n: {text}")),
            equal(column(), literal_text(text)),
            "{text}"
        );
    }
}

#[test]
fn ranges_take_literals_and_timestamps() {
    for (query, operator) in [
        ("n < 5", CompareOperator::Less),
        ("n <= 5", CompareOperator::LessOrEqual),
        ("n > 5", CompareOperator::Greater),
        ("n>=5", CompareOperator::GreaterOrEqual),
    ] {
        assert_eq!(
            parse(query),
            compare(path(vec![key("n")]), operator, integer(5)),
            "{query:?}"
        );
    }
    assert_eq!(
        parse("n > *"),
        compare(
            path(vec![key("n")]),
            CompareOperator::Greater,
            QueryValue::Text(TextPattern::new(vec![PatternPiece::AnyRun]))
        )
    );
    assert_eq!(
        parse(r#"ts >= timestamp("1700000000000", "\L")"#),
        compare(
            path(vec![key("ts")]),
            CompareOperator::GreaterOrEqual,
            QueryValue::Timestamp {
                epoch_ns: 1_700_000_000_000_000_000
            }
        )
    );
}

#[test]
fn case_sensitivity_comes_from_the_options() {
    let options = QueryOptions::default().with_case_sensitivity(false);
    let expr = LogicalExpr::parse_kql("level: error", &options).expect("query should parse");
    assert_eq!(
        expr,
        LogicalExpr::Predicate(Predicate {
            column: path(vec![key("level")]),
            test: PredicateTest::Compare {
                operator: CompareOperator::Equal,
                value: literal_text("error"),
                is_case_sensitive: false,
            },
        })
    );
}

#[test]
fn keywords_must_be_quoted_to_be_columns_or_values() {
    assert_eq!(
        parse("\"not\": \"and\""),
        equal(path(vec![key("not")]), literal_text("and"))
    );
    for query in ["not: a", "a: and", "a: OR"] {
        assert!(
            matches!(parse_error(query), QueryError::Syntax { .. }),
            "{query:?}"
        );
    }
}

#[test]
fn syntax_errors_report_byte_positions() {
    for (query, expected) in [
        ("", 0),
        ("   ", 3),
        ("a b", 2),
        ("a: 1 AND", 8),
        ("(a: 1", 5),
        ("a: 1)", 4),
        ("a: )", 3),
        ("a : , b", 4),
    ] {
        match parse_error(query) {
            QueryError::Syntax { position, .. } => assert_eq!(position, expected, "{query:?}"),
            error => panic!("{query:?} should be a syntax error, got {error}"),
        }
    }
    assert!(matches!(
        parse_error("a: \"x\\zy\""),
        QueryError::InvalidEscape { position: 5 }
    ));
    assert!(matches!(
        parse_error("a: x\\zy"),
        QueryError::InvalidEscape { position: 4 }
    ));
}

#[test]
fn the_depth_limit_bounds_hostile_nesting() {
    let options = QueryOptions {
        limits: QueryLimits {
            max_depth: 8,
            ..QueryLimits::default()
        },
        ..QueryOptions::default()
    };
    let within = format!("{}a: 1", "NOT ".repeat(7));
    assert!(LogicalExpr::parse_kql(&within, &options).is_ok());
    let beyond = format!("{}a: 1", "NOT ".repeat(8));
    assert!(matches!(
        LogicalExpr::parse_kql(&beyond, &options),
        Err(QueryError::LimitExceeded {
            actual: 9,
            limit: 8,
            ..
        })
    ));

    let alternating = (0..10)
        .map(|index| format!("a: {index}"))
        .collect::<Vec<_>>()
        .chunks(2)
        .map(|pair| pair.join(" AND "))
        .collect::<Vec<_>>()
        .join(" OR ");
    assert!(matches!(
        LogicalExpr::parse_kql(&alternating, &options),
        Err(QueryError::LimitExceeded { .. })
    ));

    let parenthesized = format!("{}a: 1{}", "(".repeat(100_000), ")".repeat(100_000));
    assert_eq!(
        LogicalExpr::parse_kql(&parenthesized, &QueryOptions::default())
            .expect("parentheses add no depth"),
        equal(path(vec![key("a")]), integer(1))
    );

    let deep = format!("{}a: 1", "NOT ".repeat(1_000_000));
    assert!(matches!(
        LogicalExpr::parse_kql(&deep, &QueryOptions::default()),
        Err(QueryError::LimitExceeded { .. })
    ));
}

#[test]
fn long_flat_chains_have_constant_depth() {
    let query = (0..50_000)
        .map(|index| format!("a: {index}"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let expr = parse(&query);
    assert_eq!(expr.depth(), 2);
}
