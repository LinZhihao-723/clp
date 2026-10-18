//! The query-suite gate of the reference evaluator.
//!
//! Every KQL query of `tests/queries/suite.toml` must parse. For every fixture query, the reference
//! evaluator runs over the C++ `clp-s x` output of the fixture's archives (the stored values) and
//! must select exactly the records of the C++ `clp-s s` oracle, or, for a query tagged with a
//! registered divergence, the records the suite expects from Rust. The dataset queries run the same
//! way over the dataset `x` oracles in an ignored test (they are large).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use clp_s_query::ColumnPattern;
use clp_s_query::JsonRecord;
use clp_s_query::NormalExpr;
use clp_s_query::PathToken;
use clp_s_query::Query;
use clp_s_query::QueryOptions;
use clp_s_query::ScalarValue;
use clp_s_query::TimeBounds;
use clp_s_query::TruthValue;

/// Fixture queries the evaluator cannot check, with the reason.
const SKIPPED: &[(&str, &str)] = &[
    (
        "range-ts-string-column",
        "string timestamps outside ISO 8601",
    ),
    ("time-nested-key", "string timestamps outside ISO 8601"),
    ("time-string-patterns", "string timestamps outside ISO 8601"),
    (
        "time-string-patterns-upper",
        "string timestamps outside ISO 8601",
    ),
    (
        "time-text-literal-on-ts",
        "string timestamps outside ISO 8601",
    ),
    (
        "wire-cache-nolog",
        "results-cache top-k is an execution-layer operator",
    ),
    (
        "wire-cache-split",
        "results-cache top-k is an execution-layer operator",
    ),
    (
        "wire-cache-topk",
        "results-cache top-k is an execution-layer operator",
    ),
    ("wire-msgpack-split", "output-shape divergence DV-21"),
];

/// The records of one fixture: the C++ `x` output lines with their parsed values and stored
/// timestamps.
struct Fixture {
    input: String,

    /// Whether the archives have a range index (C++ writes none with `--disable-log-order`).
    has_range_index: bool,

    timestamp_key: Option<ColumnPattern>,
    records: Vec<Record>,
    has_unknown_timestamps: bool,
}

/// One record of a fixture: its parsed value and its stored timestamp.
struct Record {
    value: serde_json::Value,
    timestamp_ns: Option<i64>,
}

/// The outcome of checking one fixture query.
#[derive(Debug)]
enum Outcome {
    Checked,
    Skipped(&'static str),
}

/// # Returns
///
/// The root directory of the prototype workspace.
fn prototype_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// # Returns
///
/// The directory of large artifacts: `$PROTO_DATA_ROOT`, else the workspace's `target/proto-data`.
fn data_root() -> PathBuf {
    std::env::var_os("PROTO_DATA_ROOT").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/proto-data"),
        PathBuf::from,
    )
}

/// Reads and parses the TOML file at `path`.
///
/// # Returns
///
/// The top-level table of the file on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`fs::read_to_string`]'s return values on failure.
/// * Forwards [`str::parse`]'s return values on failure.
fn load_toml(path: &Path) -> anyhow::Result<toml::Table> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    text.parse::<toml::Table>()
        .with_context(|| format!("parsing {}", path.display()))
}

/// # Returns
///
/// The tables of the array of tables at `key` of `table`, or nothing if there is no such array.
fn array_of_tables<'table>(table: &'table toml::Table, key: &str) -> Vec<&'table toml::Table> {
    table
        .get(key)
        .and_then(toml::Value::as_array)
        .map(|values| values.iter().filter_map(toml::Value::as_table).collect())
        .unwrap_or_default()
}

/// # Returns
///
/// The string at `key` of `table`, or `None` if `key` doesn't hold a string.
fn string_field<'table>(table: &'table toml::Table, key: &str) -> Option<&'table str> {
    table.get(key).and_then(toml::Value::as_str)
}

/// # Returns
///
/// The timestamp key that the `clp-s c` flags `flags` set on success, or `None` if they set none.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if `--timestamp-key` is the last flag.
/// * Forwards [`ColumnPattern::parse`]'s return values on failure.
fn timestamp_key_of(flags: &[String]) -> anyhow::Result<Option<ColumnPattern>> {
    let Some(index) = flags.iter().position(|flag| "--timestamp-key" == flag) else {
        return Ok(None);
    };
    let key = flags
        .get(index + 1)
        .context("--timestamp-key without a value")?;
    Ok(Some(ColumnPattern::parse(key)?))
}

/// # Returns
///
/// The epoch integer `value` of unknown precision in nanoseconds (C++
/// `estimate_timestamp_precision`), or `None` if the conversion overflows.
const fn integer_epoch_ns(value: i64) -> Option<i64> {
    const EPOCH_1971_S: u64 = 31_536_000;
    let magnitude = value.unsigned_abs();
    let factor = if magnitude > EPOCH_1971_S * 1_000_000_000 {
        1
    } else if magnitude > EPOCH_1971_S * 1_000_000 {
        1_000
    } else if magnitude > EPOCH_1971_S * 1_000 {
        1_000_000
    } else {
        1_000_000_000
    };
    value.checked_mul(factor)
}

/// # Returns
///
/// Epoch seconds written as a decimal (`1700000006.999999`) in nanoseconds, or `None` if `text`
/// isn't such a decimal or the value overflows.
fn decimal_seconds_ns(text: &str) -> Option<i64> {
    let (sign, digits) = text.strip_prefix('-').map_or((1, text), |rest| (-1, rest));
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    if !whole
        .bytes()
        .chain(fraction.bytes())
        .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let seconds: i64 = whole.parse().ok()?;
    let mut fraction_ns = 0_i64;
    for index in 0..9 {
        let digit = fraction
            .as_bytes()
            .get(index)
            .map_or(0, |byte| i64::from(byte - b'0'));
        fraction_ns = fraction_ns * 10 + digit;
    }
    Some(sign * (seconds.checked_mul(1_000_000_000)? + fraction_ns))
}

/// # Returns
///
/// The days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Parses `YYYY-MM-DD[T ]HH:MM:SS[.,fraction][Z|±HH:MM|±HHMM]` (also with `/` date separators
/// and inside brackets), or an all-digit epoch string.
///
/// # Returns
///
/// The parsed timestamp in epoch nanoseconds, or `None` if `text` has neither form.
fn iso_8601_ns(text: &str) -> Option<i64> {
    if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) {
        return integer_epoch_ns(text.parse().ok()?);
    }
    let text = text
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(text);
    let bytes = text.as_bytes();
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let field = text.get(range)?;
        field
            .bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| field.parse().ok())?
    };
    if bytes.len() < 19
        || !matches!(bytes[4], b'-' | b'/')
        || bytes[4] != bytes[7]
        || !matches!(bytes[10], b'T' | b' ')
        || b':' != bytes[13]
        || b':' != bytes[16]
    {
        return None;
    }
    let days = days_from_civil(number(0..4)?, number(5..7)?, number(8..10)?);
    let seconds = days * 86_400 + number(11..13)? * 3_600 + number(14..16)? * 60 + number(17..19)?;
    let mut rest = &text[19..];
    let mut fraction_ns = 0;
    if let Some(fraction) = rest.strip_prefix(['.', ',']) {
        let num_digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        fraction_ns = decimal_seconds_ns(&format!("0.{}", &fraction[..num_digits]))?;
        rest = &fraction[num_digits..];
    }
    let offset_seconds = match rest.as_bytes() {
        [] | [b'Z'] => 0,
        [sign @ (b'+' | b'-'), ..] => {
            let digits: String = rest[1..].chars().filter(char::is_ascii_digit).collect();
            if 4 != digits.len() {
                return None;
            }
            let offset =
                digits[..2].parse::<i64>().ok()? * 3_600 + digits[2..].parse::<i64>().ok()? * 60;
            if b'-' == *sign { -offset } else { offset }
        }
        _ => return None,
    };
    Some((seconds - offset_seconds) * 1_000_000_000 + fraction_ns)
}

/// # Returns
///
/// The raw text of the number at top-level key `key` of a compact JSON line, or `None` if the
/// line has no such key.
fn top_level_number_text<'line>(line: &'line str, key: &str) -> Option<&'line str> {
    let needle = format!("\"{key}\":");
    let bytes = line.as_bytes();
    let mut depth = 0_u32;
    let mut is_in_string = false;
    let mut is_escaped = false;
    for (index, &byte) in bytes.iter().enumerate() {
        if is_in_string {
            match (is_escaped, byte) {
                (true, _) => is_escaped = false,
                (false, b'\\') => is_escaped = true,
                (false, b'"') => is_in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth = depth.saturating_sub(1),
            b'"' => {
                if 1 == depth && line[index..].starts_with(&needle) {
                    let value = &line[index + needle.len()..];
                    let end = value
                        .find(|character: char| {
                            !matches!(character, '0'..='9' | '-' | '+' | '.' | 'e' | 'E')
                        })
                        .unwrap_or(value.len());
                    return Some(&value[..end]);
                }
                is_in_string = true;
            }
            _ => {}
        }
    }
    None
}

/// # Returns
///
/// The stored timestamp of a record on success, or `None` without a timestamp-typed value at the
/// key.
///
/// # Errors
///
/// Returns `()` if the test cannot derive the stored timestamp.
fn stored_timestamp_ns(
    line: &str,
    value: &serde_json::Value,
    key: &ColumnPattern,
) -> Result<Option<i64>, ()> {
    let mut current = value;
    for token in &*key.tokens {
        let PathToken::Key(key) = token else {
            return Err(());
        };
        let key = std::str::from_utf8(key).map_err(|_| ())?;
        match current.get(key) {
            Some(child) => current = child,
            None => return Ok(None),
        }
    }
    match current {
        serde_json::Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                return integer_epoch_ns(integer).map(Some).ok_or(());
            }
            let [PathToken::Key(name)] = &*key.tokens else {
                return Err(());
            };
            let name = std::str::from_utf8(name).map_err(|_| ())?;
            top_level_number_text(line, name)
                .and_then(decimal_seconds_ns)
                .map(Some)
                .ok_or(())
        }
        serde_json::Value::String(text) => iso_8601_ns(text).map(Some).ok_or(()),
        _ => Ok(None),
    }
}

/// Loads the records of a fixture from its C++ `x` oracles.
///
/// # Returns
///
/// A pair on success, containing:
///
/// * The records.
/// * Whether some record's stored timestamp can't be derived.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`fs::read_to_string`]'s return values on failure.
/// * Forwards [`serde_json::from_str`]'s return values on failure.
fn load_records(
    oracle_paths: &[PathBuf],
    timestamp_key: Option<&ColumnPattern>,
) -> anyhow::Result<(Vec<Record>, bool)> {
    let mut records = Vec::new();
    let mut has_unknown_timestamps = false;
    for path in oracle_paths {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        for line in text.lines().filter(|line| !line.is_empty()) {
            let value: serde_json::Value = serde_json::from_str(line)
                .with_context(|| format!("parsing a line of {}", path.display()))?;
            let timestamp_ns = timestamp_key.and_then(|key| {
                stored_timestamp_ns(line, &value, key).unwrap_or_else(|()| {
                    has_unknown_timestamps = true;
                    None
                })
            });
            records.push(Record {
                value,
                timestamp_ns,
            });
        }
    }
    Ok((records, has_unknown_timestamps))
}

/// Loads every fixture of the fixture manifest under `prototype`.
///
/// # Returns
///
/// The fixtures keyed by name on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`load_toml`]'s return values on failure.
/// * Forwards [`timestamp_key_of`]'s return values on failure.
/// * Forwards [`load_records`]'s return values on failure.
fn load_fixtures(prototype: &Path) -> anyhow::Result<BTreeMap<String, Fixture>> {
    let manifest = load_toml(&prototype.join("tests/fixtures/manifest.toml"))?;
    let mut archives: BTreeMap<String, (String, Vec<String>, Vec<PathBuf>)> = BTreeMap::new();
    for archive in array_of_tables(&manifest, "archive") {
        let (Some(fixture), Some(oracle)) = (
            string_field(archive, "fixture"),
            string_field(archive, "oracle"),
        ) else {
            continue;
        };
        let input = string_field(archive, "input")
            .unwrap_or_default()
            .to_owned();
        let flags = archive
            .get("flags")
            .and_then(toml::Value::as_array)
            .map(|flags| {
                flags
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let entry = archives
            .entry(fixture.to_owned())
            .or_insert_with(|| (input, flags, Vec::new()));
        entry.2.push(prototype.join("tests/fixtures").join(oracle));
    }
    let mut fixtures = BTreeMap::new();
    for (name, (input, flags, oracle_paths)) in archives {
        let timestamp_key = timestamp_key_of(&flags)?;
        let (records, has_unknown_timestamps) =
            load_records(&oracle_paths, timestamp_key.as_ref())?;
        fixtures.insert(
            name,
            Fixture {
                input,
                has_range_index: !flags.iter().any(|flag| "--disable-log-order" == flag),
                timestamp_key,
                records,
                has_unknown_timestamps,
            },
        );
    }
    Ok(fixtures)
}

/// # Returns
///
/// The normalized query of a suite entry, with its case sensitivity and time bounds, on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if the entry has no `kql`.
/// * Forwards [`Query::from_kql`]'s return values on failure.
fn query_of(query: &toml::Table) -> anyhow::Result<Query> {
    let kql = string_field(query, "kql").context("query without kql")?;
    let is_case_sensitive = !query
        .get("ignore_case")
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);
    let options = QueryOptions::default().with_case_sensitivity(is_case_sensitive);
    let parsed = Query::from_kql(kql, &options).with_context(|| format!("parsing {kql:?}"))?;
    let bounds = TimeBounds::from_epoch_millis(
        query.get("tge").and_then(toml::Value::as_integer),
        query.get("tle").and_then(toml::Value::as_integer),
    );
    Ok(parsed.with_time_bounds(bounds))
}

/// # Returns
///
/// The canonical text of `value`, which equal values share.
fn canonical(value: &serde_json::Value) -> String {
    value.to_string()
}

/// # Returns
///
/// The key of the record id in the records of `fixture`.
fn id_key(fixture: &str) -> &'static str {
    if fixture.starts_with("search.") {
        "rid"
    } else if fixture.starts_with("keys.") {
        "kid"
    } else {
        "id"
    }
}

/// # Returns
///
/// The JSON value of a TOML string or integer, or the TOML text of any other value as a JSON
/// string.
fn toml_to_json(value: &toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(text) => serde_json::Value::String(text.clone()),
        toml::Value::Integer(integer) => serde_json::Value::from(*integer),
        other => serde_json::Value::String(other.to_string()),
    }
}

/// # Returns
///
/// The records of `fixture` on which the reference evaluator finds `query` true.
fn matching<'fixture>(query: &Query, fixture: &'fixture Fixture) -> Vec<&'fixture Record> {
    let fields = serde_json::json!({ "_filename": fixture.input });
    fixture
        .records
        .iter()
        .filter(|record| {
            let timestamp = record
                .timestamp_ns
                .map(|epoch_ns| ScalarValue::Timestamp { epoch_ns });
            let mut source = JsonRecord::new(&record.value, timestamp);
            if fixture.has_range_index {
                source = source.with_range_index_fields(&fields);
            }
            if let Some(key) = &fixture.timestamp_key {
                source = source.with_timestamp_key(key);
            }
            TruthValue::True == query.evaluate(&source)
        })
        .collect()
}

/// # Returns
///
/// The canonical text of every record of the JSON-lines oracle at `path` on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`fs::read_to_string`]'s return values on failure.
/// * Forwards [`serde_json::from_str`]'s return values on failure.
fn oracle_lines(path: &Path) -> anyhow::Result<Vec<String>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    text.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .map(|value| canonical(&value))
                .with_context(|| format!("parsing a line of {}", path.display()))
        })
        .collect()
}

/// Checks a query tagged with a registered divergence against the Rust result the suite states.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if `selected` differs from the stated result, or the query states none.
/// * Forwards [`i64::try_from`]'s return values on failure.
fn check_divergent_query(
    id: &str,
    target: &str,
    query: &toml::Table,
    selected: &[&Record],
) -> anyhow::Result<()> {
    if let Some(expected) = query
        .get("expected_rust_ids")
        .and_then(toml::Value::as_array)
    {
        let key = id_key(target);
        let mut actual: Vec<String> = selected
            .iter()
            .map(|record| record.value.get(key).map(canonical).unwrap_or_default())
            .collect();
        let mut expected: Vec<String> = expected
            .iter()
            .map(|value| canonical(&toml_to_json(value)))
            .collect();
        actual.sort();
        expected.sort();
        anyhow::ensure!(
            actual == expected,
            "{id}: selected ids {actual:?}, expected {expected:?}"
        );
        return Ok(());
    }
    if let Some(expected) = query
        .get("expected_rust_count")
        .and_then(toml::Value::as_integer)
    {
        let actual = i64::try_from(selected.len())?;
        anyhow::ensure!(
            actual == expected,
            "{id}: selected {actual} records, expected {expected}"
        );
        return Ok(());
    }
    if query.contains_key("expected_rust_exit") {
        anyhow::ensure!(
            selected.is_empty(),
            "{id}: expected no records, got {}",
            selected.len()
        );
        return Ok(());
    }
    anyhow::bail!("{id}: divergent query without an expectation");
}

/// Checks one fixture query of the suite against its C++ oracle or its registered divergence.
///
/// # Returns
///
/// The outcome of the check on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if:
///   * The query lacks a field the check needs, or names an unknown fixture.
///   * The fixture has timestamps the test can't derive.
///   * The selected records differ from the oracle, or the query has no oracle.
/// * Forwards [`query_of`]'s return values on failure.
/// * Forwards [`check_divergent_query`]'s return values on failure.
/// * Forwards [`oracle_lines`]'s return values on failure.
/// * Forwards [`i64::try_from`]'s return values on failure.
fn check_fixture_query(
    prototype: &Path,
    query: &toml::Table,
    oracles: &BTreeMap<(String, String), &toml::Table>,
    fixtures: &BTreeMap<String, Fixture>,
) -> anyhow::Result<Outcome> {
    let id = string_field(query, "id").context("query without id")?;
    let target = string_field(query, "target").context("query without target")?;
    if let Some((_, reason)) = SKIPPED.iter().find(|(skipped, _)| *skipped == id) {
        return Ok(Outcome::Skipped(reason));
    }
    if query.contains_key("projection") {
        return Ok(Outcome::Skipped("projection (planner layer)"));
    }
    let fixture = fixtures
        .get(target)
        .with_context(|| format!("unknown fixture {target}"))?;
    let parsed = query_of(query)?;
    let is_match_all = NormalExpr::MatchAll == *parsed.root();
    anyhow::ensure!(
        !fixture.has_unknown_timestamps || is_match_all,
        "{id}: fixture {target} has timestamps the test cannot derive; list the query in SKIPPED"
    );
    let selected = matching(&parsed, fixture);

    if query.contains_key("divergence") {
        check_divergent_query(id, target, query, &selected)?;
        return Ok(Outcome::Checked);
    }

    if let Some(oracle) = oracles.get(&(id.to_owned(), "stdout".to_owned())) {
        let file = string_field(oracle, "file").context("oracle without file")?;
        let mut expected = oracle_lines(&prototype.join("tests/queries").join(file))?;
        let mut actual: Vec<String> = selected
            .iter()
            .map(|record| canonical(&record.value))
            .collect();
        expected.sort();
        actual.sort();
        if actual != expected {
            let missing: Vec<&String> = expected
                .iter()
                .filter(|line| !actual.contains(line))
                .collect();
            let extra: Vec<&String> = actual
                .iter()
                .filter(|line| !expected.contains(line))
                .collect();
            anyhow::bail!(
                "{id}: {} records selected, C++ selected {}\n  missing: {missing:?}\n  extra: \
                 {extra:?}",
                actual.len(),
                expected.len()
            );
        }
        return Ok(Outcome::Checked);
    }
    if let Some(oracle) = oracles.get(&(id.to_owned(), "msgpack".to_owned())) {
        let expected = oracle
            .get("count")
            .and_then(toml::Value::as_integer)
            .context("oracle without count")?;
        let actual = i64::try_from(selected.len())?;
        anyhow::ensure!(
            actual == expected,
            "{id}: selected {actual} records, C++ wrote {expected} tuples"
        );
        return Ok(Outcome::Checked);
    }
    anyhow::bail!("{id}: no oracle to compare with")
}

/// # Returns
///
/// A pair on success, containing:
///
/// * The query suite under `prototype`.
/// * The manifest of its oracles.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`load_toml`]'s return values on failure.
fn load_suite(prototype: &Path) -> anyhow::Result<(toml::Table, toml::Table)> {
    let suite = load_toml(&prototype.join("tests/queries/suite.toml"))?;
    let oracles = load_toml(&prototype.join("tests/queries/oracles/manifest.toml"))?;
    Ok((suite, oracles))
}

/// # Returns
///
/// The oracles of the oracle manifest `oracles`, keyed by query id and output.
fn index_oracles(oracles: &toml::Table) -> BTreeMap<(String, String), &toml::Table> {
    array_of_tables(oracles, "oracle")
        .into_iter()
        .filter_map(|oracle| {
            Some((
                (
                    string_field(oracle, "id")?.to_owned(),
                    string_field(oracle, "output")?.to_owned(),
                ),
                oracle,
            ))
        })
        .collect()
}

#[test]
fn every_suite_query_parses() -> anyhow::Result<()> {
    let (suite, _) = load_suite(&prototype_dir())?;
    let queries = array_of_tables(&suite, "query");
    anyhow::ensure!(
        queries.len() > 300,
        "the suite should have its 308 queries, found {}",
        queries.len()
    );
    for query in queries {
        query_of(query)?;
    }
    Ok(())
}

#[test]
fn fixture_queries_match_the_cxx_oracles_or_the_registered_divergences() -> anyhow::Result<()> {
    let prototype = prototype_dir();
    let (suite, oracles) = load_suite(&prototype)?;
    let oracles = index_oracles(&oracles);
    let fixtures = load_fixtures(&prototype)?;
    let mut failures = Vec::new();
    let mut num_checked = 0;
    let mut skipped = Vec::new();
    for query in array_of_tables(&suite, "query") {
        let target = string_field(query, "target").unwrap_or_default();
        if target.starts_with("dataset:") {
            continue;
        }
        match check_fixture_query(&prototype, query, &oracles, &fixtures) {
            Ok(Outcome::Checked) => num_checked += 1,
            Ok(Outcome::Skipped(reason)) => {
                skipped.push(format!(
                    "{}: {reason}",
                    string_field(query, "id").unwrap_or_default()
                ));
            }
            Err(error) => failures.push(format!("{error:#}")),
        }
    }
    let ids: Vec<&str> = array_of_tables(&suite, "query")
        .into_iter()
        .filter_map(|query| string_field(query, "id"))
        .collect();
    for (id, _) in SKIPPED {
        assert!(ids.contains(id), "skipped query {id} is not in the suite");
    }
    println!(
        "checked {num_checked} fixture queries; skipped {}:",
        skipped.len()
    );
    for line in &skipped {
        println!("  {line}");
    }
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        num_checked > 250,
        "only {num_checked} fixture queries were checked"
    );
    Ok(())
}

#[test]
#[ignore = "reads the multi-GB dataset oracles under PROTO_DATA_ROOT; run with --release --ignored"]
fn dataset_queries_match_the_cxx_counts_or_the_registered_divergences() -> anyhow::Result<()> {
    use std::io::BufRead;

    let prototype = prototype_dir();
    let (suite, oracles) = load_suite(&prototype)?;
    let oracles = index_oracles(&oracles);
    let datasets = load_toml(&prototype.join("tests/queries/datasets.toml"))?;
    let mut failures = Vec::new();
    for dataset in array_of_tables(&datasets, "dataset") {
        let name = string_field(dataset, "name").context("dataset without name")?;
        let x_oracle = data_root()
            .join(string_field(dataset, "x_oracle").context("dataset without x oracle")?);
        let flags: Vec<String> = dataset
            .get("flags")
            .and_then(toml::Value::as_array)
            .map(|flags| {
                flags
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let timestamp_key = timestamp_key_of(&flags)?;
        let target = format!("dataset:{name}");
        let mut queries = Vec::new();
        for query in array_of_tables(&suite, "query") {
            if Some(target.as_str()) == string_field(query, "target") {
                queries.push((query, query_of(query)?, 0_i64));
            }
        }

        let reader = std::io::BufReader::new(
            fs::File::open(&x_oracle).with_context(|| format!("opening {}", x_oracle.display()))?,
        );
        let mut num_unknown_timestamps = 0_u64;
        for line in reader.lines() {
            let line = line?;
            let value: serde_json::Value = serde_json::from_str(&line)?;
            let timestamp_ns = timestamp_key.as_ref().and_then(|key| {
                stored_timestamp_ns(&line, &value, key).unwrap_or_else(|()| {
                    num_unknown_timestamps += 1;
                    None
                })
            });
            let timestamp = timestamp_ns.map(|epoch_ns| ScalarValue::Timestamp { epoch_ns });
            let mut source = JsonRecord::new(&value, timestamp);
            if let Some(key) = &timestamp_key {
                source = source.with_timestamp_key(key);
            }
            for (_, query, count) in &mut queries {
                if TruthValue::True == query.evaluate(&source) {
                    *count += 1;
                }
            }
        }
        anyhow::ensure!(
            0 == num_unknown_timestamps,
            "{name}: {num_unknown_timestamps} unparsed timestamps"
        );

        for (query, _, actual) in queries {
            let id = string_field(query, "id").unwrap_or_default();
            let expected = if query.contains_key("divergence") {
                query
                    .get("expected_rust_count")
                    .and_then(toml::Value::as_integer)
            } else {
                oracles
                    .get(&(id.to_owned(), "stdout".to_owned()))
                    .and_then(|oracle| oracle.get("count"))
                    .and_then(toml::Value::as_integer)
            };
            println!("{id}: {actual} (expected {expected:?})");
            if Some(actual) != expected {
                failures.push(format!("{id}: selected {actual}, expected {expected:?}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
    Ok(())
}
