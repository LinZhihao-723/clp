//! The C++ ground-truth corpus.
//!
//! It is written by `scripts/build-fixtures.sh` and `scripts/gen-oracles.sh`:
//! `tests/fixtures/fixtures.toml` and its oracle manifest, `tests/queries/suite.toml` and its
//! oracle manifests, and `tests/queries/datasets.toml`.

use std::path::Path;
use std::path::PathBuf;

use crate::reader::SearchOptions;

/// The default directory of large artifacts (dataset archives and oracles, perf results): the
/// workspace's `target/proto-data`.
pub const DEFAULT_DATA_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/proto-data");

/// What the Rust reader must do with a fixture archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Expectation {
    Readable,

    /// Fail at open with `UnsupportedArchive`.
    Reject,
}

/// How a fixture was built (`fixtures.toml`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FixtureKind {
    /// A single-file archive compressed from JSON lines.
    Sfa,

    /// A single-file archive compressed from a kv-IR stream.
    Kvir,

    /// A multi-file archive directory (rejected: SX-01).
    RejectMfa,

    /// A single-file archive with structured arrays (rejected: SX-02).
    RejectSfa,
}

/// One decompression oracle: a C++-written archive and its C++ `clp-s x` output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveOracle {
    /// The fixture name in `fixtures.toml`.
    pub fixture: String,

    pub kind: FixtureKind,
    pub expectation: Expectation,

    /// The absolute archive path (a directory for a multi-file archive).
    pub archive: PathBuf,

    /// The `clp-s c` flags the archive was built with.
    pub flags: Vec<String>,

    /// The absolute path of the C++ output, or `None` for an archive the Rust reader must
    /// reject.
    pub oracle: Option<PathBuf>,

    /// The canonical digest of the oracle (`canonical_sha256`).
    pub canonical_sha256: Option<String>,

    /// The number of records C++ decompresses (also for rejected archives, which C++ reads).
    pub num_records: u64,
}

impl ArchiveOracle {
    /// # Returns
    ///
    /// The archive's file name, which is its archive id.
    #[must_use]
    pub fn archive_name(&self) -> String {
        self.archive
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// One fixture of `tests/fixtures/archives/index.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fixture {
    pub name: String,
    pub kind: FixtureKind,

    /// The input file name in `tests/fixtures/inputs/`.
    pub input: String,

    pub flags: Vec<String>,

    /// The absolute paths of its archives, in creation order.
    pub archives: Vec<PathBuf>,
}

impl Fixture {
    /// # Returns
    ///
    /// The `--timestamp-key` the fixture was compressed with, if any.
    #[must_use]
    pub fn timestamp_key(&self) -> Option<&str> {
        let position = self
            .flags
            .iter()
            .position(|flag| flag == "--timestamp-key")?;
        self.flags.get(position + 1).map(String::as_str)
    }

    /// # Returns
    ///
    /// Whether the fixture's archives have a range index (C++ writes none with
    /// `--disable-log-order`).
    #[must_use]
    pub fn has_range_index(&self) -> bool {
        !self.flags.iter().any(|flag| flag == "--disable-log-order")
    }
}

/// Where a query runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryTarget {
    /// Every archive of a fixture, as one session.
    Fixture(String),

    /// A dataset archive.
    Dataset(String),
}

/// An output a query case is checked on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputKind {
    /// JSON lines on stdout.
    Stdout,

    /// C++ `file --path` msgpack tuples.
    Msgpack,

    /// Results-cache documents (top-k).
    ResultsCache,
}

impl std::fmt::Display for OutputKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Stdout => "stdout",
            Self::Msgpack => "msgpack",
            Self::ResultsCache => "results-cache",
        })
    }
}

/// The value of a record's id field (`rid`, `kid` or `id`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Deserialize)]
#[serde(untagged)]
pub enum RecordKey {
    Integer(i64),
    Text(String),
}

impl std::fmt::Display for RecordKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Integer(value) => write!(formatter, "{value}"),
            Self::Text(text) => write!(formatter, "{text:?}"),
        }
    }
}

/// One `[[query]]` of `tests/queries/suite.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryCase {
    pub id: String,
    pub target: QueryTarget,
    pub kql: String,
    pub options: SearchOptions,
    pub outputs: Vec<OutputKind>,
    pub max_num_results: u64,
    pub dataset_name: Option<String>,

    /// The registered divergences the case declares.
    pub divergences: Vec<String>,

    pub expected_rust_ids: Option<Vec<RecordKey>>,
    pub expected_rust_count: Option<u64>,
    pub expected_rust_exit: Option<i32>,
}

impl QueryCase {
    /// # Returns
    ///
    /// The field that identifies a record of the case's fixture (`gen-oracles.sh`'s `id_key`).
    #[must_use]
    pub fn id_key(&self) -> &'static str {
        match &self.target {
            QueryTarget::Fixture(name) if name.starts_with("search.") => "rid",
            QueryTarget::Fixture(name) if name.starts_with("keys.") => "kid",
            QueryTarget::Fixture(_) | QueryTarget::Dataset(_) => "id",
        }
    }

    /// # Returns
    ///
    /// Whether the case declares at least one divergence.
    #[must_use]
    pub const fn is_divergent(&self) -> bool {
        !self.divergences.is_empty()
    }
}

/// One `[[oracle]]` of `tests/queries/oracles/manifest.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryOracle {
    pub id: String,
    pub output: OutputKind,

    /// The paths of the searched archives, in session order.
    pub archives: Vec<PathBuf>,

    /// The path of the C++ output.
    pub file: PathBuf,

    pub cpp_exit: i32,

    /// The number of result lines, tuples or documents.
    pub count: u64,

    pub canonical_sha256: Option<String>,

    /// The record ids C++ returned.
    pub cpp_ids: Option<Vec<RecordKey>>,

    /// The record ids `scripts/reference_eval.py` returned.
    pub reference_ids: Option<Vec<RecordKey>>,

    pub reference_status: Option<String>,
}

/// One `[[dataset]]` of `tests/queries/datasets.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatasetOracle {
    pub name: String,
    pub archive: PathBuf,
    pub flags: Vec<String>,

    /// The C++ `clp-s x` output.
    pub x_oracle: PathBuf,

    pub x_records: u64,
    pub x_canonical_sha256: String,
}

impl DatasetOracle {
    /// # Returns
    ///
    /// The `--timestamp-key` the dataset was compressed with, if any.
    #[must_use]
    pub fn timestamp_key(&self) -> Option<&str> {
        let position = self
            .flags
            .iter()
            .position(|flag| flag == "--timestamp-key")?;
        self.flags.get(position + 1).map(String::as_str)
    }
}

/// The whole corpus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Corpus {
    /// The prototype root.
    pub root: PathBuf,

    /// The directory of large artifacts that dataset paths are relative to.
    pub data_root: PathBuf,

    pub archive_oracles: Vec<ArchiveOracle>,
    pub fixtures: Vec<Fixture>,
    pub queries: Vec<QueryCase>,
    pub query_oracles: Vec<QueryOracle>,
    pub datasets: Vec<DatasetOracle>,
}

impl Corpus {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The corpus under the prototype root `root`, with dataset paths under [`data_root`], on
    /// success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::load_with_data_root`]'s return values on failure.
    pub fn load(root: &Path) -> anyhow::Result<Self> {
        Self::load_with_data_root(root, &data_root())
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The corpus under the prototype root `root`, with dataset paths under `data_root`, on
    /// success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`load_fixture_oracles`]'s return values on failure.
    /// * Forwards [`load_fixtures`]'s return values on failure.
    /// * Forwards [`load_queries`]'s return values on failure.
    /// * Forwards [`load_query_oracles`]'s return values on failure.
    /// * Forwards [`load_datasets`]'s return values on failure.
    pub fn load_with_data_root(root: &Path, data_root: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            root: root.to_path_buf(),
            data_root: data_root.to_path_buf(),
            archive_oracles: load_fixture_oracles(root)?,
            fixtures: load_fixtures(root)?,
            queries: load_queries(root)?,
            query_oracles: load_query_oracles(root, data_root)?,
            datasets: load_datasets(root, data_root)?,
        })
    }

    /// # Returns
    ///
    /// The fixture named `name`, if any.
    #[must_use]
    pub fn fixture(&self, name: &str) -> Option<&Fixture> {
        self.fixtures.iter().find(|fixture| fixture.name == name)
    }

    /// # Returns
    ///
    /// The C++ oracle of `id` for `output`, if any.
    #[must_use]
    pub fn query_oracle(&self, id: &str, output: OutputKind) -> Option<&QueryOracle> {
        self.query_oracles
            .iter()
            .find(|oracle| oracle.id == id && oracle.output == output)
    }

    /// # Returns
    ///
    /// The dataset named `name`, if any.
    #[must_use]
    pub fn dataset(&self, name: &str) -> Option<&DatasetOracle> {
        self.datasets.iter().find(|dataset| dataset.name == name)
    }
}

/// The prototype root (the directory of the workspace `Cargo.toml`).
///
/// # Returns
///
/// The absolute path of the prototype root.
#[must_use]
pub fn prototype_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The directory of large artifacts: `$PROTO_DATA_ROOT`, else [`DEFAULT_DATA_ROOT`].
///
/// # Returns
///
/// The path of the data root.
#[must_use]
pub fn data_root() -> PathBuf {
    std::env::var_os("PROTO_DATA_ROOT")
        .map_or_else(|| PathBuf::from(DEFAULT_DATA_ROOT), PathBuf::from)
}

/// Loads every decompression oracle of the fixture corpus.
///
/// # Returns
///
/// The oracles in manifest order on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if a readable archive has no oracle or digest.
/// * Forwards [`read_toml`]'s return values on failure.
pub fn load_fixture_oracles(root: &Path) -> anyhow::Result<Vec<ArchiveOracle>> {
    #[derive(serde::Deserialize)]
    struct Manifest {
        archive: Vec<Row>,
    }

    #[derive(serde::Deserialize)]
    struct Row {
        fixture: String,
        kind: FixtureKind,
        file: String,
        #[serde(default)]
        flags: Vec<String>,
        expect: Expectation,
        oracle: Option<String>,
        records: Option<u64>,
        oracle_canonical_sha256: Option<String>,
        cpp_x_records: Option<u64>,
    }

    let fixtures_dir = root.join("tests/fixtures");
    let manifest: Manifest = read_toml(&fixtures_dir.join("manifest.toml"))?;
    manifest
        .archive
        .into_iter()
        .map(|row| {
            if row.expect == Expectation::Readable
                && (row.oracle.is_none() || row.oracle_canonical_sha256.is_none())
            {
                anyhow::bail!("readable fixture archive {} has no oracle", row.file);
            }
            Ok(ArchiveOracle {
                fixture: row.fixture,
                kind: row.kind,
                expectation: row.expect,
                archive: fixtures_dir.join(&row.file),
                flags: row.flags,
                oracle: row.oracle.map(|oracle| fixtures_dir.join(oracle)),
                canonical_sha256: row.oracle_canonical_sha256,
                num_records: row.records.or(row.cpp_x_records).unwrap_or_default(),
            })
        })
        .collect()
}

/// Loads the fixture index (`tests/fixtures/archives/index.toml`).
///
/// # Returns
///
/// The fixtures in index order on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`read_toml`]'s return values on failure.
pub fn load_fixtures(root: &Path) -> anyhow::Result<Vec<Fixture>> {
    #[derive(serde::Deserialize)]
    struct Index {
        archive: Vec<Row>,
    }

    #[derive(serde::Deserialize)]
    struct Row {
        fixture: String,
        kind: FixtureKind,
        input: String,
        #[serde(default)]
        flags: Vec<String>,
        files: Vec<String>,
    }

    let fixtures_dir = root.join("tests/fixtures");
    let index: Index = read_toml(&fixtures_dir.join("archives/index.toml"))?;
    Ok(index
        .archive
        .into_iter()
        .map(|row| Fixture {
            name: row.fixture,
            kind: row.kind,
            input: row.input,
            flags: row.flags,
            archives: row
                .files
                .iter()
                .map(|file| fixtures_dir.join(file))
                .collect(),
        })
        .collect())
}

/// Loads the query suite (`tests/queries/suite.toml`).
///
/// # Returns
///
/// The query cases in suite order on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if a query id is repeated.
/// * Forwards [`read_toml`]'s return values on failure.
pub fn load_queries(root: &Path) -> anyhow::Result<Vec<QueryCase>> {
    #[derive(serde::Deserialize)]
    struct Suite {
        query: Vec<Row>,
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Row {
        id: String,
        target: String,
        kql: String,
        #[serde(default)]
        ignore_case: bool,
        tge: Option<i64>,
        tle: Option<i64>,
        #[serde(default)]
        projection: Vec<String>,
        outputs: Option<Vec<OutputKind>>,
        max_num_results: Option<u64>,
        dataset_name: Option<String>,
        #[serde(default)]
        divergence: Vec<String>,
        expected_rust_ids: Option<Vec<RecordKey>>,
        expected_rust_count: Option<u64>,
        expected_rust_exit: Option<i32>,
        #[serde(rename = "note")]
        _note: Option<String>,
    }

    const DEFAULT_MAX_NUM_RESULTS: u64 = 10;
    let suite: Suite = read_toml(&root.join("tests/queries/suite.toml"))?;
    let mut ids = std::collections::HashSet::new();
    suite
        .query
        .into_iter()
        .map(|row| {
            if !ids.insert(row.id.clone()) {
                anyhow::bail!("query id {} is repeated", row.id);
            }
            let target = row.target.strip_prefix("dataset:").map_or_else(
                || QueryTarget::Fixture(row.target.clone()),
                |name| QueryTarget::Dataset(name.to_owned()),
            );
            Ok(QueryCase {
                id: row.id,
                target,
                kql: row.kql,
                options: SearchOptions {
                    time_begin_ms: row.tge,
                    time_end_ms: row.tle,
                    is_case_insensitive: row.ignore_case,
                    projection: row.projection,
                },
                outputs: row.outputs.unwrap_or_else(|| vec![OutputKind::Stdout]),
                max_num_results: row.max_num_results.unwrap_or(DEFAULT_MAX_NUM_RESULTS),
                dataset_name: row.dataset_name,
                divergences: row.divergence,
                expected_rust_ids: row.expected_rust_ids,
                expected_rust_count: row.expected_rust_count,
                expected_rust_exit: row.expected_rust_exit,
            })
        })
        .collect()
}

/// Loads the query oracle manifest (`tests/queries/oracles/manifest.toml`), resolving the paths of
/// dataset queries against `data_root`.
///
/// # Returns
///
/// The oracles in manifest order on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`read_toml`]'s return values on failure.
pub fn load_query_oracles(root: &Path, data_root: &Path) -> anyhow::Result<Vec<QueryOracle>> {
    #[derive(serde::Deserialize)]
    struct Manifest {
        oracle: Vec<Row>,
    }

    #[derive(serde::Deserialize)]
    struct Row {
        id: String,
        target: String,
        output: OutputKind,
        archives: Vec<String>,
        file: String,
        cpp_exit: i32,
        count: u64,
        canonical_sha256: Option<String>,
        cpp_ids: Option<Vec<RecordKey>>,
        reference_ids: Option<Vec<RecordKey>>,
        reference_status: Option<String>,
    }

    let fixtures_dir = root.join("tests/fixtures");
    let queries_dir = root.join("tests/queries");
    let manifest: Manifest = read_toml(&queries_dir.join("oracles/manifest.toml"))?;
    Ok(manifest
        .oracle
        .into_iter()
        .map(|row| {
            let (archive_dir, file_dir) = if row.target.starts_with("dataset:") {
                (data_root, data_root)
            } else {
                (fixtures_dir.as_path(), queries_dir.as_path())
            };
            QueryOracle {
                id: row.id,
                output: row.output,
                archives: row
                    .archives
                    .iter()
                    .map(|archive| archive_dir.join(archive))
                    .collect(),
                file: file_dir.join(row.file),
                cpp_exit: row.cpp_exit,
                count: row.count,
                canonical_sha256: row.canonical_sha256,
                cpp_ids: row.cpp_ids,
                reference_ids: row.reference_ids,
                reference_status: row.reference_status,
            }
        })
        .collect())
}

/// Loads the dataset manifest (`tests/queries/datasets.toml`), resolving its paths against
/// `data_root`.
///
/// # Returns
///
/// The datasets in manifest order on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`read_toml`]'s return values on failure.
pub fn load_datasets(root: &Path, data_root: &Path) -> anyhow::Result<Vec<DatasetOracle>> {
    #[derive(serde::Deserialize)]
    struct Manifest {
        dataset: Vec<Row>,
    }

    #[derive(serde::Deserialize)]
    struct Row {
        name: String,
        archive: PathBuf,
        #[serde(default)]
        flags: Vec<String>,
        x_oracle: PathBuf,
        x_records: u64,
        x_canonical_sha256: String,
    }

    let manifest: Manifest = read_toml(&root.join("tests/queries/datasets.toml"))?;
    Ok(manifest
        .dataset
        .into_iter()
        .map(|row| DatasetOracle {
            name: row.name,
            archive: data_root.join(row.archive),
            flags: row.flags,
            x_oracle: data_root.join(row.x_oracle),
            x_records: row.x_records,
            x_canonical_sha256: row.x_canonical_sha256,
        })
        .collect())
}

/// Reads and deserializes a TOML file.
///
/// # Type Parameters
///
/// * `DocumentType` - The document shape.
///
/// # Returns
///
/// The document on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if the file cannot be read or does not match `DocumentType`.
pub fn read_toml<DocumentType: serde::de::DeserializeOwned>(
    path: &Path,
) -> anyhow::Result<DocumentType> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| anyhow::anyhow!("cannot read {}: {error}", path.display()))?;
    toml::from_str(&text)
        .map_err(|error| anyhow::anyhow!("cannot parse {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::Corpus;
    use super::Expectation;
    use super::Fixture;
    use super::OutputKind;
    use super::QueryTarget;
    use super::RecordKey;
    use super::prototype_root;

    #[test]
    fn the_corpus_loads_with_the_documented_counts() -> anyhow::Result<()> {
        let corpus = Corpus::load(&prototype_root())?;
        let num_readable = corpus
            .archive_oracles
            .iter()
            .filter(|oracle| oracle.expectation == Expectation::Readable)
            .count();
        assert_eq!((num_readable, corpus.archive_oracles.len()), (57, 59));
        assert_eq!(corpus.queries.len(), 342);
        assert_eq!(corpus.datasets.len(), 3);
        for oracle in &corpus.archive_oracles {
            assert!(oracle.archive.exists(), "{}", oracle.archive.display());
            if let Some(path) = &oracle.oracle {
                assert!(path.exists(), "{}", path.display());
            }
        }
        Ok(())
    }

    #[test]
    fn every_query_has_a_target_and_an_oracle_per_output() -> anyhow::Result<()> {
        let corpus = Corpus::load(&prototype_root())?;
        for case in &corpus.queries {
            match &case.target {
                QueryTarget::Fixture(name) => {
                    assert!(
                        corpus.fixture(name).is_some(),
                        "{}: fixture {name}",
                        case.id
                    );
                }
                QueryTarget::Dataset(name) => {
                    assert!(
                        corpus.dataset(name).is_some(),
                        "{}: dataset {name}",
                        case.id
                    );
                }
            }
            for output in &case.outputs {
                let oracle = corpus.query_oracle(&case.id, *output);
                assert!(oracle.is_some(), "{} [{output}] has no oracle", case.id);
            }
        }
        Ok(())
    }

    #[test]
    fn query_fields_are_parsed() -> anyhow::Result<()> {
        let corpus = Corpus::load(&prototype_root())?;
        let case = |id: &str| {
            corpus
                .queries
                .iter()
                .find(|case| case.id == id)
                .expect("the suite has this case")
        };
        let projection = case("bench-projection-ci");
        assert!(projection.options.is_case_insensitive);
        assert_eq!(
            projection.options.projection,
            ["timestamp", "level", "service"]
        );
        assert_eq!(projection.target, QueryTarget::Dataset("bench".to_owned()));
        let range = case("bench-ts-range");
        assert_eq!(
            (range.options.time_begin_ms, range.options.time_end_ms),
            (Some(1_735_694_600_000), Some(1_735_694_700_000))
        );
        let cache = case("wire-cache-split");
        assert_eq!(cache.outputs, [OutputKind::ResultsCache]);
        assert_eq!(cache.max_num_results, 10);
        assert_eq!(cache.divergences, ["DV-25"]);
        assert_eq!(case("time-no-ts-key").expected_rust_exit, Some(0));
        assert_eq!(case("bench-not-bare").expected_rust_count, Some(750_396));
        assert_eq!(case("eq-int").outputs, [OutputKind::Stdout]);
        Ok(())
    }

    #[test]
    fn id_keys_follow_the_fixture_family() -> anyhow::Result<()> {
        let corpus = Corpus::load(&prototype_root())?;
        let mut seen = std::collections::HashSet::new();
        for case in &corpus.queries {
            let expected = match &case.target {
                QueryTarget::Fixture(name) if name.starts_with("search.") => "rid",
                QueryTarget::Fixture(name) if name.starts_with("keys.") => "kid",
                _ => "id",
            };
            assert_eq!(case.id_key(), expected, "{}", case.id);
            seen.insert(expected);
        }
        assert_eq!(seen.len(), 3);
        let integer_ids = corpus
            .queries
            .iter()
            .filter_map(|case| case.expected_rust_ids.as_ref())
            .flatten()
            .any(|id| matches!(id, RecordKey::Integer(_)));
        assert!(integer_ids, "the suite has integer expected ids");
        Ok(())
    }

    #[test]
    fn fixture_timestamp_keys_come_from_the_flags() -> anyhow::Result<()> {
        let corpus = Corpus::load(&prototype_root())?;
        let key = |name: &str| {
            corpus
                .fixture(name)
                .and_then(|fixture| fixture.timestamp_key())
        };
        assert_eq!(key("search.ts"), Some("ts"));
        assert_eq!(key("timestamps-string.ts-nested"), Some("meta.time"));
        assert_eq!(key("search.default"), None);
        Ok(())
    }

    #[test]
    fn only_archives_without_log_order_lack_a_range_index() -> anyhow::Result<()> {
        let corpus = Corpus::load(&prototype_root())?;
        let has_range_index = |name: &str| corpus.fixture(name).map(Fixture::has_range_index);
        assert_eq!(has_range_index("search.ts"), Some(true));
        assert_eq!(has_range_index("search.ts-nolog"), Some(false));
        assert_eq!(has_range_index("many-schemas.nolog"), Some(false));
        Ok(())
    }
}
