//! The Rust reference evaluator (`clp_s_query::Query::evaluate`) over the records of a fixture or
//! a dataset.
//!
//! Fixture records are loaded as `scripts/reference_eval.py` loads them: the C++
//! `clp-s s '*' file --path` tuples, with the authoritative timestamp derived from the record text
//! and the C++ epoch milliseconds. Dataset records are the JSON lines of the dataset's `x` oracle,
//! evaluated in one streaming pass for every query of the dataset.

use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;

use clp_s_query::ColumnPattern;
use clp_s_query::JsonRecord;
use clp_s_query::Query;
use clp_s_query::QueryOptions;
use clp_s_query::ScalarValue;
use clp_s_query::TimeBounds;
use clp_s_query::TruthValue;

use crate::canonical::JsonValue;
use crate::canonical::canonical_record;
use crate::corpus::DatasetOracle;
use crate::corpus::Fixture;
use crate::corpus::QueryCase;
use crate::corpus::RecordKey;
use crate::cpp::CppClpS;
use crate::cpp::CppOutput;
use crate::multiset::MultisetFingerprint;
use crate::reader::SearchOptions;
use crate::reader::WireTuple;

/// The records of a fixture, in C++ scan order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferenceRecords {
    records: Vec<ReferenceRecord>,
    timestamp_key: Option<ColumnPattern>,

    /// The `$` namespace of every record (`_filename` is the fixture's input file), or `None` if
    /// the archives have no range index.
    range_index_fields: Option<serde_json::Value>,
}

impl ReferenceRecords {
    /// Factory function.
    ///
    /// Loads every record of the fixture's archives through C++ `clp-s s '*' file --path`.
    ///
    /// # Returns
    ///
    /// The records on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if C++ fails.
    /// * Forwards [`ColumnPattern::parse`]'s return values on failure.
    /// * Forwards [`CppClpS::search`]'s return values on failure.
    /// * Forwards [`WireTuple::decode_stream`]'s return values on failure.
    /// * Forwards [`ReferenceRecord::from_tuple`]'s return values on failure.
    pub fn load(cpp: &CppClpS, fixture: &Fixture, work_dir: &Path) -> anyhow::Result<Self> {
        let timestamp_key = fixture
            .timestamp_key()
            .map(ColumnPattern::parse)
            .transpose()?;
        let mut records = Vec::new();
        for (index, archive) in fixture.archives.iter().enumerate() {
            let path = work_dir.join(format!("reference-{index}.msgpack"));
            let run = cpp.search(
                archive,
                "*",
                &SearchOptions::default(),
                &CppOutput::File { path },
            )?;
            if run.exit_code != 0 {
                anyhow::bail!(
                    "clp-s s {} '*' exited with {}: {}",
                    archive.display(),
                    run.exit_code,
                    run.stderr
                );
            }
            for tuple in WireTuple::decode_stream(&run.output)? {
                records.push(ReferenceRecord::from_tuple(&tuple, timestamp_key.as_ref())?);
            }
        }
        Ok(Self {
            records,
            timestamp_key,
            range_index_fields: fixture
                .has_range_index()
                .then(|| serde_json::json!({ "_filename": fixture.input })),
        })
    }

    /// Evaluates a query case on every record.
    ///
    /// # Returns
    ///
    /// The matching records, in C++ scan order, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Query::from_kql`]'s return values on failure.
    pub fn matching_records(&self, case: &QueryCase) -> anyhow::Result<Vec<&ReferenceRecord>> {
        let options =
            QueryOptions::default().with_case_sensitivity(!case.options.is_case_insensitive);
        let query = Query::from_kql(&case.kql, &options)?.with_time_bounds(
            TimeBounds::from_epoch_millis(case.options.time_begin_ms, case.options.time_end_ms),
        );
        Ok(self
            .records
            .iter()
            .filter(|record| {
                let timestamp = record
                    .timestamp_ns
                    .map(|epoch_ns| ScalarValue::Timestamp { epoch_ns });
                let mut source = JsonRecord::new(&record.value, timestamp);
                if let Some(fields) = &self.range_index_fields {
                    source = source.with_range_index_fields(fields);
                }
                if let Some(timestamp_key) = &self.timestamp_key {
                    source = source.with_timestamp_key(timestamp_key);
                }
                query.evaluate(&source) == TruthValue::True
            })
            .collect())
    }

    /// Evaluates a query case on every record.
    ///
    /// # Returns
    ///
    /// The sorted values of `id_key` of the matching records on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::matching_records`]'s return values on failure.
    pub fn matching_ids(&self, case: &QueryCase, id_key: &str) -> anyhow::Result<Vec<RecordKey>> {
        let mut ids: Vec<RecordKey> = self
            .matching_records(case)?
            .into_iter()
            .filter_map(|record| record_key(record.value.get(id_key)?))
            .collect();
        ids.sort();
        Ok(ids)
    }

    /// # Returns
    ///
    /// The number of records.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.records.len()
    }

    /// # Returns
    ///
    /// Whether there is no record.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// One record: its value (u64 integers wrapped to i64, as C++ stores them), its authoritative
/// timestamp, and the C++ message it was parsed from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferenceRecord {
    pub value: serde_json::Value,
    pub timestamp_ns: Option<i64>,

    /// The C++ JSON text of the record, without its newline.
    pub message: Vec<u8>,
}

impl ReferenceRecord {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The record of a C++ result tuple on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if the message is not JSON.
    pub fn from_tuple(
        tuple: &WireTuple,
        timestamp_key: Option<&ColumnPattern>,
    ) -> anyhow::Result<Self> {
        let message = tuple.message.strip_suffix(b"\n").unwrap_or(&tuple.message);
        let raw = JsonValue::parse(message).ok_or_else(|| {
            anyhow::anyhow!("message is not JSON: {}", String::from_utf8_lossy(message))
        })?;
        let timestamp_ns = timestamp_key
            .and_then(|key| raw.member_at(key))
            .and_then(|leaf| stored_timestamp_ns(leaf, tuple.timestamp_ms));
        Ok(Self {
            value: to_serde_value(&raw),
            timestamp_ns,
            message: message.to_vec(),
        })
    }
}

/// The reference results of the queries of one dataset, from one streaming pass over the records
/// of its `x` oracle.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DatasetReference {
    /// The fingerprint of the matching records of each query, by query id.
    results: HashMap<String, MultisetFingerprint>,
}

impl DatasetReference {
    /// Factory function.
    ///
    /// Evaluates every case of `cases` on every record of the dataset. The authoritative timestamp
    /// of a record is the value at the dataset's `--timestamp-key`: an integer (its unit inferred
    /// like the C++ ingestion) or an ISO-8601 UTC string (`YYYY-MM-DDTHH:MM:SS[.fraction]Z`).
    ///
    /// # Returns
    ///
    /// The results on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if the oracle cannot be opened, or a record is not JSON.
    /// * Forwards [`Query::from_kql`]'s return values on failure.
    /// * Forwards [`ColumnPattern::parse`]'s return values on failure.
    /// * Forwards [`BufRead::read_until`]'s return values on failure.
    pub fn evaluate(dataset: &DatasetOracle, cases: &[&QueryCase]) -> anyhow::Result<Self> {
        let queries = cases
            .iter()
            .map(|case| {
                let options = QueryOptions::default()
                    .with_case_sensitivity(!case.options.is_case_insensitive);
                Ok(Query::from_kql(&case.kql, &options)?.with_time_bounds(
                    TimeBounds::from_epoch_millis(
                        case.options.time_begin_ms,
                        case.options.time_end_ms,
                    ),
                ))
            })
            .collect::<anyhow::Result<Vec<Query>>>()?;
        let timestamp_key = dataset
            .timestamp_key()
            .map(ColumnPattern::parse)
            .transpose()?;
        let mut fingerprints = vec![MultisetFingerprint::default(); cases.len()];
        let file = std::fs::File::open(&dataset.x_oracle).map_err(|error| {
            anyhow::anyhow!("cannot open {}: {error}", dataset.x_oracle.display())
        })?;
        let mut reader = std::io::BufReader::with_capacity(1 << 20, file);
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }
            let raw = JsonValue::parse(&line).ok_or_else(|| {
                anyhow::anyhow!("record is not JSON: {}", String::from_utf8_lossy(&line))
            })?;
            let timestamp = timestamp_key
                .as_ref()
                .and_then(|key| raw.member_at(key))
                .and_then(dataset_timestamp_ns)
                .map(|epoch_ns| ScalarValue::Timestamp { epoch_ns });
            let value = to_serde_value(&raw);
            let mut source = JsonRecord::new(&value, timestamp);
            if let Some(timestamp_key) = &timestamp_key {
                source = source.with_timestamp_key(timestamp_key);
            }
            let mut canonical = None;
            for (query, fingerprint) in queries.iter().zip(&mut fingerprints) {
                if query.evaluate(&source) == TruthValue::True {
                    fingerprint.insert(canonical.get_or_insert_with(|| canonical_record(&line)));
                }
            }
        }
        Ok(Self {
            results: cases
                .iter()
                .map(|case| case.id.clone())
                .zip(fingerprints)
                .collect(),
        })
    }

    /// # Returns
    ///
    /// The fingerprint of the records query `id` matches, if it was evaluated.
    #[must_use]
    pub fn result(&self, id: &str) -> Option<&MultisetFingerprint> {
        self.results.get(id)
    }
}

/// Derives the epoch nanoseconds C++ stored for a timestamp value from its text and the C++
/// epoch milliseconds (`reference_eval.py`'s `_ts_ns`).
///
/// # Returns
///
/// The epoch nanoseconds, or `None` if `value` is not timestamp-typed (a number or a string) or
/// the result overflows `i64`.
#[must_use]
pub fn stored_timestamp_ns(value: &JsonValue, timestamp_ms: i64) -> Option<i64> {
    const NANOSECONDS_PER_MILLISECOND: i128 = 1_000_000;
    let digits: &[u8] = match value {
        JsonValue::Number(token)
            if !token.iter().any(|byte| matches!(byte, b'.' | b'e' | b'E')) =>
        {
            let integer: i128 = std::str::from_utf8(token).ok()?.parse().ok()?;
            return i64::try_from(integer_timestamp_ns(integer)).ok();
        }
        JsonValue::Number(token) => token
            .iter()
            .position(|byte| *byte == b'.')
            .filter(|_| !token.iter().any(|byte| matches!(byte, b'e' | b'E')))
            .map_or(&[], |dot| &token[dot + 1..]),
        JsonValue::String(text) => fraction_digits(text),
        _ => return None,
    };
    let sub_millisecond: i128 = if digits.len() > 3 {
        let mut padded = digits[3..digits.len().min(9)].to_vec();
        padded.resize(6, b'0');
        std::str::from_utf8(&padded).ok()?.parse().ok()?
    } else {
        0
    };
    let base = i128::from(timestamp_ms) * NANOSECONDS_PER_MILLISECOND;
    let signed = if timestamp_ms >= 0 {
        sub_millisecond
    } else {
        -sub_millisecond
    };
    i64::try_from(base + signed).ok()
}

/// # Returns
///
/// The epoch nanoseconds of a dataset timestamp value: an integer (see [`integer_timestamp_ns`])
/// or an ISO-8601 UTC string, or `None` for any other value.
fn dataset_timestamp_ns(value: &JsonValue) -> Option<i64> {
    match value {
        JsonValue::Number(token)
            if token
                .iter()
                .all(|byte| byte.is_ascii_digit() || b'-' == *byte) =>
        {
            let integer: i128 = std::str::from_utf8(token).ok()?.parse().ok()?;
            i64::try_from(integer_timestamp_ns(integer)).ok()
        }
        JsonValue::String(text) => iso8601_utc_epoch_ns(text),
        _ => None,
    }
}

/// Parses `YYYY-MM-DDTHH:MM:SS[.fraction]Z` (at most nine fraction digits).
///
/// # Returns
///
/// The epoch nanoseconds, or `None` if `text` has another shape.
fn iso8601_utc_epoch_ns(text: &[u8]) -> Option<i64> {
    const NANOSECONDS_PER_SECOND: i64 = 1_000_000_000;
    let field = |range: std::ops::Range<usize>| -> Option<i64> {
        let digits = text.get(range)?;
        if !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(digits).ok()?.parse().ok()
    };
    let separators = [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':')];
    if separators
        .iter()
        .any(|(position, separator)| text.get(*position) != Some(separator))
    {
        return None;
    }
    let (year, month, day) = (field(0..4)?, field(5..7)?, field(8..10)?);
    let (hour, minute, second) = (field(11..13)?, field(14..16)?, field(17..19)?);
    let rest = text.get(19..)?;
    let fraction = match rest {
        [b'Z'] => &[][..],
        [b'.', digits @ .., b'Z'] if (1..=9).contains(&digits.len()) => digits,
        _ => return None,
    };
    if !fraction.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut fraction_ns = 0;
    for position in 0..9 {
        fraction_ns = fraction_ns * 10
            + fraction
                .get(position)
                .map_or(0, |byte| i64::from(byte - b'0'));
    }
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second;
    seconds
        .checked_mul(NANOSECONDS_PER_SECOND)?
        .checked_add(fraction_ns)
}

/// # Returns
///
/// The number of days from 1970-01-01 to the proleptic Gregorian date `year-month-day` (Howard
/// Hinnant's `days_from_civil`).
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// # Returns
///
/// The epoch nanoseconds of an integer timestamp, its unit inferred from its magnitude like the
/// C++ ingestion (`reference_eval.py`'s `_int_ts_ns`).
const fn integer_timestamp_ns(value: i128) -> i128 {
    const EPOCH_1971_SECONDS: i128 = 365 * 24 * 3600;
    let magnitude = value.abs();
    if magnitude > EPOCH_1971_SECONDS * 1_000_000_000 {
        value
    } else if magnitude > EPOCH_1971_SECONDS * 1_000_000 {
        value * 1000
    } else if magnitude > EPOCH_1971_SECONDS * 1000 {
        value * 1_000_000
    } else {
        value * 1_000_000_000
    }
}

/// # Returns
///
/// The digits after the first `[0-9]{2}[.,]` of `text` (the fraction of a date string), or an
/// empty slice if there is none.
fn fraction_digits(text: &[u8]) -> &[u8] {
    for start in 0..text.len().saturating_sub(2) {
        let window = &text[start..];
        if window[0].is_ascii_digit()
            && window[1].is_ascii_digit()
            && matches!(window[2], b'.' | b',')
        {
            let fraction = &window[3..];
            let size = fraction
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if size > 0 {
                return &fraction[..size];
            }
        }
    }
    &[]
}

/// Converts a parsed record to the `serde_json` value the reference evaluator reads, as
/// `reference_eval.py` loads it: integers wrap modulo 2^64 to `i64` (as C++ ingestion stores
/// them), and floats that overflow `f64` become the largest finite `f64` of their sign, since
/// `serde_json` cannot hold infinities.
///
/// # Returns
///
/// The value.
fn to_serde_value(value: &JsonValue) -> serde_json::Value {
    match value {
        JsonValue::Null => serde_json::Value::Null,
        JsonValue::Boolean(boolean) => serde_json::Value::Bool(*boolean),
        JsonValue::Number(token) => number_value(token),
        JsonValue::String(text) => {
            serde_json::Value::String(String::from_utf8_lossy(text).into_owned())
        }
        JsonValue::Array(elements) => {
            serde_json::Value::Array(elements.iter().map(to_serde_value).collect())
        }
        JsonValue::Object(members) => serde_json::Value::Object(
            members
                .iter()
                .map(|(key, member)| {
                    (
                        String::from_utf8_lossy(key).into_owned(),
                        to_serde_value(member),
                    )
                })
                .collect(),
        ),
    }
}

/// # Returns
///
/// The `serde_json` number of a JSON number token (see [`to_serde_value`]).
fn number_value(token: &[u8]) -> serde_json::Value {
    let text = String::from_utf8_lossy(token);
    let is_integer = !token.iter().any(|byte| matches!(byte, b'.' | b'e' | b'E'));
    if is_integer && let Ok(integer) = text.parse::<i128>() {
        let wrapped = integer.rem_euclid(1 << 64);
        let bits = u64::try_from(wrapped).unwrap_or_default();
        return serde_json::Value::Number(i64::from_ne_bytes(bits.to_ne_bytes()).into());
    }
    let float = text.parse::<f64>().unwrap_or(f64::NAN);
    let finite = if float.is_infinite() {
        f64::MAX.copysign(float)
    } else {
        float
    };
    serde_json::Number::from_f64(finite).map_or(serde_json::Value::Null, serde_json::Value::Number)
}

/// # Returns
///
/// The record key of an id value, if it is an integer or a string.
fn record_key(value: &serde_json::Value) -> Option<RecordKey> {
    value.as_i64().map_or_else(
        || value.as_str().map(|text| RecordKey::Text(text.to_owned())),
        |integer| Some(RecordKey::Integer(integer)),
    )
}

#[cfg(test)]
mod tests {
    use clp_s_query::ColumnPattern;

    use super::ReferenceRecord;
    use super::ReferenceRecords;
    use super::dataset_timestamp_ns;
    use super::iso8601_utc_epoch_ns;
    use super::stored_timestamp_ns;
    use crate::canonical::JsonValue;
    use crate::corpus::Corpus;
    use crate::corpus::prototype_root;
    use crate::cpp::CppClpS;
    use crate::reader::WireTuple;

    /// Timestamp values, C++ epoch milliseconds, and the epoch nanoseconds `reference_eval.py`'s
    /// `_ts_ns` derives from them.
    const TIMESTAMP_VECTORS: &[(&str, i64, i64)] = &[
        ("1700000000", 1_700_000_000_000, 1_700_000_000_000_000_000),
        (
            "1700000000123",
            1_700_000_000_123,
            1_700_000_000_123_000_000,
        ),
        (
            "1700000000123456",
            1_700_000_000_123,
            1_700_000_000_123_456_000,
        ),
        (
            "1700000000123456789",
            1_700_000_000_123,
            1_700_000_000_123_456_789,
        ),
        ("0", 0, 0),
        ("-5", -5000, -5_000_000_000),
        (
            "1700000000.123456789",
            1_700_000_000_123,
            1_700_000_000_123_456_789,
        ),
        ("1700000000.5", 1_700_000_000_500, 1_700_000_000_500_000_000),
        ("-1.123456", -1123, -1_123_456_000),
        ("1.5e3", 1500, 1_500_000_000),
        (
            "\"2023-11-14T22:13:20.123456Z\"",
            1_700_000_000_123,
            1_700_000_000_123_456_000,
        ),
        (
            "\"2023-11-14 22:13:20,987654321\"",
            1_700_000_000_987,
            1_700_000_000_987_654_321,
        ),
        (
            "\"14 Nov 2023 22:13:20\"",
            1_700_000_000_000,
            1_700_000_000_000_000_000,
        ),
        (
            "\"2023-11-14T22:13:20.12Z\"",
            1_700_000_000_120,
            1_700_000_000_120_000_000,
        ),
    ];

    #[test]
    fn stored_timestamps_match_reference_eval_py() {
        for (text, timestamp_ms, expected_ns) in TIMESTAMP_VECTORS {
            let value = JsonValue::parse(text.as_bytes()).expect("valid JSON");
            assert_eq!(
                stored_timestamp_ns(&value, *timestamp_ms),
                Some(*expected_ns),
                "{text}"
            );
        }
    }

    #[test]
    fn dataset_timestamps_are_integers_or_iso8601_utc_strings() {
        for (text, expected_ns) in [
            ("1735689600009", Some(1_735_689_600_009_000_000)),
            (
                "\"2026-08-25T21:56:43.323386Z\"",
                Some(1_787_695_003_323_386_000),
            ),
            (
                "\"2026-01-01T00:00:09.896Z\"",
                Some(1_767_225_609_896_000_000),
            ),
            ("\"1970-01-01T00:00:00Z\"", Some(0)),
            ("\"1969-12-31T23:59:59.5Z\"", Some(-500_000_000)),
            (
                "\"2024-02-29T12:00:00.123456789Z\"",
                Some(1_709_208_000_123_456_789),
            ),
            ("\"2026-01-01 00:00:09Z\"", None),
            ("\"2026-01-01T00:00:09.Z\"", None),
            ("\"2026-01-01T00:00:09.1234567890Z\"", None),
            ("\"2026-01-01T00:00:09+00:00\"", None),
            ("\"2026-01-01T00:00:09.1x3Z\"", None),
            ("1.5", None),
            ("true", None),
        ] {
            let value = JsonValue::parse(text.as_bytes()).expect("valid JSON");
            assert_eq!(dataset_timestamp_ns(&value), expected_ns, "{text}");
        }
        assert_eq!(iso8601_utc_epoch_ns(b"2026-01-01T00:00:0xZ"), None);
    }

    #[test]
    fn non_timestamp_values_have_no_stored_timestamp() {
        for text in ["true", "null", "{}", "[1]"] {
            let value = JsonValue::parse(text.as_bytes()).expect("valid JSON");
            assert_eq!(stored_timestamp_ns(&value, 5), None, "{text}");
        }
    }

    #[test]
    fn records_wrap_u64_and_take_the_timestamp_at_the_key() -> anyhow::Result<()> {
        let tuple = WireTuple {
            timestamp_ms: 1_700_000_000_123,
            message: b"{\"u\":18446744073709551615,\"meta\":{\"time\":1700000000.123456}}\n"
                .to_vec(),
            orig_file_path: Vec::new(),
            archive_id: b"a".to_vec(),
            log_event_idx: 0,
        };
        let record =
            ReferenceRecord::from_tuple(&tuple, Some(&ColumnPattern::parse("meta.time")?))?;
        assert_eq!(record.value["u"], serde_json::json!(-1));
        let overflow = WireTuple {
            message: b"{\"f\":-1e400,\"i\":36893488147419103231,\"g\":1.5}\n".to_vec(),
            ..tuple.clone()
        };
        let value = ReferenceRecord::from_tuple(&overflow, None)?.value;
        assert_eq!(value["f"], serde_json::json!(-f64::MAX));
        assert_eq!(value["i"], serde_json::json!(-1));
        assert_eq!(value["g"], serde_json::json!(1.5));
        assert_eq!(record.timestamp_ns, Some(1_700_000_000_123_456_000));
        assert_eq!(
            ReferenceRecord::from_tuple(&tuple, Some(&ColumnPattern::parse("missing")?))?
                .timestamp_ns,
            None
        );
        assert_eq!(
            ReferenceRecord::from_tuple(&tuple, None)?.timestamp_ns,
            None
        );
        let escaped = WireTuple {
            message: b"{\"t*s\":1700000000123,\"t\\\\*s\":1}\n".to_vec(),
            ..tuple
        };
        assert_eq!(
            ReferenceRecord::from_tuple(&escaped, Some(&ColumnPattern::parse(r"t\*s")?))?
                .timestamp_ns,
            Some(1_700_000_000_123_000_000)
        );
        Ok(())
    }

    #[test]
    fn load_reads_every_record_of_a_fixture() -> anyhow::Result<()> {
        let corpus = Corpus::load(&prototype_root())?;
        let fixture = corpus
            .fixture("search.ts")
            .expect("the corpus has search.ts");
        let work = tempfile::tempdir()?;
        let records = ReferenceRecords::load(&CppClpS::default(), fixture, work.path())?;
        assert_eq!(records.len(), 40);
        assert!(
            records
                .records
                .iter()
                .filter(|record| record.timestamp_ns.is_some())
                .count()
                > 30
        );
        Ok(())
    }
}
