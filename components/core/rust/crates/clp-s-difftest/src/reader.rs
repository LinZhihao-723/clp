//! The reader under test: what the harness asks of an implementation (the C++ `clp-s` for the
//! self-test, the Rust reader from M1 on) and the outcomes it compares.

use std::path::Path;
use std::path::PathBuf;

use clp_s_reader::ErrorKind;

use crate::canonical::canonical_record;
use crate::corpus::OutputKind;

/// The exit code `clp-s-rs` (and the C++ `clp-s` it mirrors) uses for corrupt and unsupported
/// archives.
pub const EXIT_ARCHIVE: i32 = 2;

/// An archive reader whose outputs the harness compares with the C++ oracles.
pub trait ReaderUnderTest {
    /// # Returns
    ///
    /// The reader's name in reports.
    fn name(&self) -> &str;

    /// Decompresses one archive (unordered).
    ///
    /// # Parameters
    ///
    /// * `archive` - The archive file (or a directory, for a multi-file archive).
    /// * `work_dir` - An empty directory the reader may use for its outputs.
    ///
    /// # Returns
    ///
    /// The JSON lines and, if the reader failed, the failure, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if the reader cannot be run at all (an infrastructure failure, not a
    ///   reader failure).
    fn extract(&self, archive: &Path, work_dir: &Path) -> anyhow::Result<ExtractOutcome>;

    /// Searches archives as one session.
    ///
    /// # Parameters
    ///
    /// * `request` - The archives, query, options and output.
    /// * `work_dir` - An empty directory the reader may use for its outputs.
    ///
    /// # Returns
    ///
    /// The output and, if the reader failed, the failure, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if the reader cannot be run at all (an infrastructure failure, not a
    ///   reader failure).
    fn search(&self, request: &SearchRequest<'_>, work_dir: &Path)
    -> anyhow::Result<SearchOutcome>;
}

/// The search options of a query case (C++ `--tge`, `--tle`, `-i`, `--projection`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchOptions {
    pub time_begin_ms: Option<i64>,
    pub time_end_ms: Option<i64>,
    pub is_case_insensitive: bool,
    pub projection: Vec<String>,
}

/// The requested output of a search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputRequest {
    /// JSON lines (C++ stdout).
    Stdout,

    /// Msgpack result tuples (C++ `file --path`).
    Msgpack,

    /// The results cache with a top-k of `max_num_results`.
    ResultsCache {
        max_num_results: u64,
        dataset: Option<String>,
    },
}

impl OutputRequest {
    /// # Returns
    ///
    /// The kind of this output.
    #[must_use]
    pub const fn kind(&self) -> OutputKind {
        match self {
            Self::Stdout => OutputKind::Stdout,
            Self::Msgpack => OutputKind::Msgpack,
            Self::ResultsCache { .. } => OutputKind::ResultsCache,
        }
    }
}

/// One search of a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchRequest<'request> {
    /// The archives in session order.
    pub archives: &'request [PathBuf],

    pub kql: &'request str,
    pub options: &'request SearchOptions,
    pub output: OutputRequest,
}

/// A failure of the reader under test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReaderFailure {
    /// The error kind, for a library reader.
    pub kind: Option<ErrorKind>,

    /// The exit code, for a process.
    pub exit_code: Option<i32>,

    pub message: String,
}

impl ReaderFailure {
    /// # Returns
    ///
    /// Whether the failure reports an unsupported archive: [`ErrorKind::UnsupportedArchive`],
    /// or [`EXIT_ARCHIVE`] from a process.
    #[must_use]
    pub fn is_unsupported_archive(&self) -> bool {
        self.kind.map_or_else(
            || self.exit_code == Some(EXIT_ARCHIVE),
            |kind| kind == ErrorKind::UnsupportedArchive,
        )
    }
}

impl std::fmt::Display for ReaderFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(kind) = self.kind {
            write!(formatter, "{kind:?}: ")?;
        }
        if let Some(exit_code) = self.exit_code {
            write!(formatter, "exit {exit_code}: ")?;
        }
        formatter.write_str(&self.message)
    }
}

/// The outcome of [`ReaderUnderTest::extract`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtractOutcome {
    pub jsonl: Vec<u8>,
    pub failure: Option<ReaderFailure>,
}

/// The outcome of [`ReaderUnderTest::search`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchOutcome {
    pub output: SearchOutput,
    pub failure: Option<ReaderFailure>,
}

/// What a search produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchOutput {
    JsonLines(Vec<u8>),
    Msgpack(Vec<WireTuple>),
    ResultsCache(Vec<ResultsCacheDocument>),

    /// The reader does not implement the requested output (yet).
    Unsupported,
}

/// One msgpack result tuple: `[timestamp_ms, message, orig_file_path, archive_id,
/// log_event_idx]`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WireTuple {
    pub timestamp_ms: i64,

    /// The JSON line followed by `\n`.
    pub message: Vec<u8>,

    pub orig_file_path: Vec<u8>,
    pub archive_id: Vec<u8>,
    pub log_event_idx: i64,
}

impl WireTuple {
    /// Decodes a stream of concatenated msgpack tuples (the C++ `file --path` output).
    ///
    /// # Returns
    ///
    /// The tuples in stream order on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if a tuple is malformed.
    pub fn decode_stream(data: &[u8]) -> anyhow::Result<Vec<Self>> {
        let mut tuples = Vec::new();
        let mut remaining = data;
        while !remaining.is_empty() {
            let mut deserializer = rmp_serde::Deserializer::new(&mut remaining);
            let (timestamp_ms, message, orig_file_path, archive_id, log_event_idx): (
                i64,
                ByteString,
                ByteString,
                ByteString,
                i64,
            ) = serde::Deserialize::deserialize(&mut deserializer).map_err(|error| {
                anyhow::anyhow!("malformed msgpack tuple {}: {error}", tuples.len())
            })?;
            tuples.push(Self {
                timestamp_ms,
                message: message.0,
                orig_file_path: orig_file_path.0,
                archive_id: archive_id.0,
                log_event_idx,
            });
        }
        Ok(tuples)
    }

    /// # Returns
    ///
    /// The tuple with its message canonicalized (without the trailing newline), for multiset
    /// comparison.
    #[must_use]
    pub fn canonical(&self) -> Self {
        let message = self.message.strip_suffix(b"\n").unwrap_or(&self.message);
        Self {
            message: canonical_record(message),
            orig_file_path: self.orig_file_path.clone(),
            archive_id: self.archive_id.clone(),
            ..*self
        }
    }
}

/// One results-cache document, as `gen-oracles.sh` dumps it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResultsCacheDocument {
    pub archive_id: String,
    pub log_event_idx: i64,
    pub dataset: String,

    /// The JSON line followed by `\n`.
    pub message: String,

    pub orig_file_path: String,
    pub timestamp: i64,
}

impl ResultsCacheDocument {
    /// Parses a dump of documents in `MongoDB` canonical extended JSON (`results-cache/<id>.json`).
    ///
    /// # Returns
    ///
    /// The documents on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if the dump is not an array of results-cache documents.
    pub fn parse_dump(text: &str) -> anyhow::Result<Vec<Self>> {
        let documents: Vec<serde_json::Value> = serde_json::from_str(text)?;
        documents.iter().map(Self::from_extended_json).collect()
    }

    /// # Returns
    ///
    /// The document with its message canonicalized (without the trailing newline), for
    /// multiset comparison.
    #[must_use]
    pub fn canonical(&self) -> Self {
        let message = self.message.strip_suffix('\n').unwrap_or(&self.message);
        Self {
            archive_id: self.archive_id.clone(),
            dataset: self.dataset.clone(),
            message: String::from_utf8_lossy(&canonical_record(message.as_bytes())).into_owned(),
            orig_file_path: self.orig_file_path.clone(),
            ..*self
        }
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The document parsed from one extended-JSON value on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if a field is missing or has the wrong type.
    fn from_extended_json(value: &serde_json::Value) -> anyhow::Result<Self> {
        let text = |path: &[&str]| -> anyhow::Result<String> {
            lookup(value, path)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("results-cache document lacks {}", path.join(".")))
        };
        let integer = |path: &[&str]| -> anyhow::Result<i64> {
            lookup(value, path)
                .and_then(extended_json_integer)
                .ok_or_else(|| anyhow::anyhow!("results-cache document lacks {}", path.join(".")))
        };
        Ok(Self {
            archive_id: text(&["_id", "archive_id"])?,
            log_event_idx: integer(&["_id", "log_event_idx"])?,
            dataset: text(&["dataset"])?,
            message: text(&["message"])?,
            orig_file_path: text(&["orig_file_path"])?,
            timestamp: integer(&["timestamp"])?,
        })
    }
}

/// A msgpack string or binary, kept as bytes (C++ may write invalid UTF-8, DV-45).
struct ByteString(Vec<u8>);

impl<'de> serde::Deserialize<'de> for ByteString {
    fn deserialize<DeserializerType: serde::Deserializer<'de>>(
        deserializer: DeserializerType,
    ) -> Result<Self, DeserializerType::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = ByteString;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a string or binary")
            }

            fn visit_str<ErrorType: serde::de::Error>(
                self,
                value: &str,
            ) -> Result<Self::Value, ErrorType> {
                Ok(ByteString(value.as_bytes().to_vec()))
            }

            fn visit_bytes<ErrorType: serde::de::Error>(
                self,
                value: &[u8],
            ) -> Result<Self::Value, ErrorType> {
                Ok(ByteString(value.to_vec()))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// # Returns
///
/// The value at `path` of nested objects, if any.
fn lookup<'value>(
    value: &'value serde_json::Value,
    path: &[&str],
) -> Option<&'value serde_json::Value> {
    path.iter().try_fold(value, |current, key| current.get(key))
}

/// # Returns
///
/// The integer of a plain JSON integer or of a `{"$numberLong": "..."}` / `{"$numberInt":
/// "..."}` wrapper, if `value` is one.
fn extended_json_integer(value: &serde_json::Value) -> Option<i64> {
    if let Some(integer) = value.as_i64() {
        return Some(integer);
    }
    ["$numberLong", "$numberInt"]
        .iter()
        .find_map(|key| value.get(key).and_then(serde_json::Value::as_str))
        .and_then(|text| text.parse().ok())
}

#[cfg(test)]
mod tests {
    use clp_s_reader::ErrorKind;

    use super::EXIT_ARCHIVE;
    use super::ReaderFailure;
    use super::ResultsCacheDocument;
    use super::WireTuple;
    use crate::corpus::prototype_root;

    /// One msgpack tuple whose message is a `str` holding invalid UTF-8.
    const INVALID_UTF8_TUPLE: &[u8] = &[
        0x95, 0xcd, 0x03, 0xe8, 0xa4, b'{', 0xff, b'}', b'\n', 0xa0, 0xa1, b'a', 0x07,
    ];

    #[test]
    fn decode_stream_reads_concatenated_tuples() -> anyhow::Result<()> {
        let mut data =
            rmp_serde::to_vec(&(1_700_000_000_000_i64, "{\"a\":1}\n", "", "x.clps", 3_i64))?;
        data.extend(rmp_serde::to_vec(&(-5_i64, "[]\n", "p", "y.clps", 0_i64))?);
        assert_eq!(
            WireTuple::decode_stream(&data)?,
            vec![
                WireTuple {
                    timestamp_ms: 1_700_000_000_000,
                    message: b"{\"a\":1}\n".to_vec(),
                    orig_file_path: Vec::new(),
                    archive_id: b"x.clps".to_vec(),
                    log_event_idx: 3,
                },
                WireTuple {
                    timestamp_ms: -5,
                    message: b"[]\n".to_vec(),
                    orig_file_path: b"p".to_vec(),
                    archive_id: b"y.clps".to_vec(),
                    log_event_idx: 0,
                },
            ]
        );
        Ok(())
    }

    #[test]
    fn decode_stream_keeps_invalid_utf8_bytes() -> anyhow::Result<()> {
        let tuples = WireTuple::decode_stream(INVALID_UTF8_TUPLE)?;
        assert_eq!(tuples.len(), 1);
        assert_eq!(tuples[0].timestamp_ms, 1000);
        assert_eq!(tuples[0].message, b"{\xff}\n".to_vec());
        assert_eq!(tuples[0].log_event_idx, 7);
        Ok(())
    }

    #[test]
    fn decode_stream_rejects_malformed_tuples() -> anyhow::Result<()> {
        let data = rmp_serde::to_vec(&(1_i64, "m", "", "a", 2_i64))?;
        assert!(WireTuple::decode_stream(&data[..data.len() - 1]).is_err());
        assert!(WireTuple::decode_stream(&rmp_serde::to_vec(&(1_i64, "m"))?).is_err());
        assert_eq!(WireTuple::decode_stream(&[])?, vec![]);
        Ok(())
    }

    #[test]
    fn decode_stream_reads_a_cpp_oracle() -> anyhow::Result<()> {
        let path =
            prototype_root().join("tests/queries/oracles/msgpack/wire-msgpack-search.msgpack");
        let tuples = WireTuple::decode_stream(&std::fs::read(path)?)?;
        assert_eq!(tuples.len(), 6);
        assert!(
            tuples
                .iter()
                .all(|tuple| tuple.archive_id == b"search.ts.clps")
        );
        assert!(tuples.iter().all(|tuple| tuple.message.ends_with(b"\n")));
        Ok(())
    }

    #[test]
    fn canonical_tuples_drop_the_newline_and_sort_keys() {
        let tuple = WireTuple {
            timestamp_ms: 1,
            message: b"{\"b\":1,\"a\":2}\n".to_vec(),
            orig_file_path: Vec::new(),
            archive_id: b"a".to_vec(),
            log_event_idx: 2,
        };
        assert_eq!(tuple.canonical().message, br#"{"a":2,"b":1}"#.to_vec());
        assert_eq!(tuple.canonical().log_event_idx, 2);
    }

    #[test]
    fn parse_dump_reads_extended_json() -> anyhow::Result<()> {
        let path =
            prototype_root().join("tests/queries/oracles/results-cache/wire-cache-topk.json");
        let documents = ResultsCacheDocument::parse_dump(&std::fs::read_to_string(path)?)?;
        assert_eq!(documents.len(), 10);
        assert_eq!(documents[0].archive_id, "logs.ts.clps");
        assert_eq!(documents[0].log_event_idx, 2990);
        assert_eq!(documents[0].timestamp, 1_735_691_846_509);
        assert_eq!(documents[0].dataset, "");
        let plain = concat!(
            r#"[{"_id":{"archive_id":"a","log_event_idx":{"$numberInt":"4"}},"#,
            r#""dataset":"d","message":"{}\n","orig_file_path":"","timestamp":5}]"#
        );
        let documents = ResultsCacheDocument::parse_dump(plain)?;
        assert_eq!((documents[0].log_event_idx, documents[0].timestamp), (4, 5));
        assert!(ResultsCacheDocument::parse_dump(r#"[{"_id":{}}]"#).is_err());
        Ok(())
    }

    #[test]
    fn unsupported_archive_is_recognized_by_kind_or_exit_code() {
        let failure = |kind, exit_code| ReaderFailure {
            kind,
            exit_code,
            message: String::new(),
        };
        assert!(failure(Some(ErrorKind::UnsupportedArchive), None).is_unsupported_archive());
        assert!(
            !failure(Some(ErrorKind::CorruptArchive), Some(EXIT_ARCHIVE)).is_unsupported_archive()
        );
        assert!(failure(None, Some(EXIT_ARCHIVE)).is_unsupported_archive());
        assert!(!failure(None, Some(1)).is_unsupported_archive());
    }
}
