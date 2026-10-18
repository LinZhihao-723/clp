//! The comparison driver.
//!
//! It runs a [`ReaderUnderTest`] over the corpus (fixture `x`, the query suite, the datasets) and
//! judges every case against the C++ oracles under the divergence registry (`docs/DESIGN.md` §10).

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;

use crate::canonical::CanonicalDigest;
use crate::canonical::JsonValue;
use crate::canonical::canonical_record;
use crate::compare::RecordComparison;
use crate::corpus::ArchiveOracle;
use crate::corpus::Corpus;
use crate::corpus::DatasetOracle;
use crate::corpus::Expectation;
use crate::corpus::FixtureKind;
use crate::corpus::OutputKind;
use crate::corpus::QueryCase;
use crate::corpus::QueryOracle;
use crate::corpus::QueryTarget;
use crate::corpus::RecordKey;
use crate::cpp::CppClpS;
use crate::multiset::MultisetFingerprint;
use crate::multiset::RecordMultiset;
use crate::probe::probe_archive_dependence;
use crate::reader::OutputRequest;
use crate::reader::ReaderFailure;
use crate::reader::ReaderUnderTest;
use crate::reader::ResultsCacheDocument;
use crate::reader::SearchOutcome;
use crate::reader::SearchOutput;
use crate::reader::SearchRequest;
use crate::reader::WireTuple;
use crate::reference::DatasetReference;
use crate::reference::ReferenceRecords;
use crate::registry::Classifier;
use crate::registry::Divergence;
use crate::registry::DivergenceId;
use crate::registry::Registry;

/// The default number of records of each side a failure report shows.
pub const DEFAULT_MAX_DIFF_RECORDS: usize = 10;

/// The default number of mismatching records the archive-dependence probe re-runs.
pub const DEFAULT_MAX_PROBED_RECORDS: usize = 5;

/// Harness options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessOptions {
    /// The number of records of each side a failure report shows.
    pub max_diff_records: usize,

    /// Re-run C++ on single-record archives for undeclared fixture mismatches (design report
    /// §8.3).
    pub is_archive_dependence_probe_enabled: bool,

    pub max_probed_records: usize,

    /// Check every stdout case against `clp_s_query`'s reference evaluator too: the third oracle
    /// next to C++ and the reader under test.
    pub is_reference_evaluator_enabled: bool,

    /// The case ids (query ids, fixture names, archive file names or dataset names) to run;
    /// empty runs every case.
    pub case_filter: Vec<String>,
}

impl Default for HarnessOptions {
    fn default() -> Self {
        Self {
            max_diff_records: DEFAULT_MAX_DIFF_RECORDS,
            is_archive_dependence_probe_enabled: false,
            max_probed_records: DEFAULT_MAX_PROBED_RECORDS,
            is_reference_evaluator_enabled: false,
            case_filter: Vec::new(),
        }
    }
}

/// The report of one case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaseReport {
    /// The fixture archive name, query id or `x:<dataset>`.
    pub case: String,

    pub output: Option<OutputKind>,
    pub verdict: Verdict,
}

impl std::fmt::Display for CaseReport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let output = self
            .output
            .map(|output| format!(" [{output}]"))
            .unwrap_or_default();
        match &self.verdict {
            Verdict::Pass => write!(formatter, "PASS     {}{output}", self.case),
            Verdict::Accepted {
                divergences,
                detail,
            } => {
                let ids: Vec<&str> = divergences.iter().map(DivergenceId::as_str).collect();
                write!(
                    formatter,
                    "ACCEPTED {}{output} ({}): {detail}",
                    self.case,
                    ids.join(", ")
                )
            }
            Verdict::Skipped { reason } => {
                write!(formatter, "SKIP     {}{output}: {reason}", self.case)
            }
            Verdict::Fail(failure) => {
                write!(formatter, "FAIL     {}{output}: {failure}", self.case)
            }
        }
    }
}

/// The judgement of one case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Equal to the C++ oracle.
    Pass,

    /// Different from C++ exactly as registered divergences predict.
    Accepted {
        divergences: Vec<DivergenceId>,
        detail: String,
    },

    /// Not run (e.g. the reader lacks the output).
    Skipped {
        reason: String,
    },

    Fail(Failure),
}

impl Verdict {
    /// # Returns
    ///
    /// Whether the verdict is a failure.
    #[must_use]
    pub const fn is_failure(&self) -> bool {
        matches!(self, Self::Fail(_))
    }
}

/// Why a case failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The reader could not be run, or an oracle could not be read.
    Infrastructure { error: String },

    /// The case or the corpus is inconsistent (no oracle, unregistered divergence, ...).
    InvalidCase { detail: String },

    /// The reader failed where C++ succeeded.
    ReaderFailed { failure: ReaderFailure },

    /// The output differs from the C++ oracle and no rule explains it.
    RecordMismatch { report: String },

    /// The reader read an archive it must reject.
    NotRejected { num_records: u64 },

    /// The reader rejected an archive, but not with `UnsupportedArchive` before any output.
    WrongRejection { detail: String },

    /// The case declares a divergence, but the reader's output equals C++'s.
    DivergenceNotReproduced { divergences: Vec<DivergenceId> },

    /// The output differs from C++ but not as the case's expectation states.
    ExpectationMismatch { detail: String },

    /// The case's expectation disagrees with a reference model.
    ReferenceDisagreement { detail: String },
}

impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Infrastructure { error } => write!(formatter, "infrastructure error: {error}"),
            Self::InvalidCase { detail } => write!(formatter, "invalid case: {detail}"),
            Self::ReaderFailed { failure } => write!(formatter, "reader failed: {failure}"),
            Self::RecordMismatch { report } => write!(formatter, "record mismatch: {report}"),
            Self::NotRejected { num_records } => write!(
                formatter,
                "archive must be rejected with UnsupportedArchive, but {num_records} records were \
                 read"
            ),
            Self::WrongRejection { detail } => write!(formatter, "wrong rejection: {detail}"),
            Self::DivergenceNotReproduced { divergences } => {
                let ids: Vec<&str> = divergences.iter().map(DivergenceId::as_str).collect();
                write!(
                    formatter,
                    "declared divergence {} no longer occurs: the output equals C++",
                    ids.join(", ")
                )
            }
            Self::ExpectationMismatch { detail } => {
                write!(formatter, "expectation mismatch: {detail}")
            }
            Self::ReferenceDisagreement { detail } => {
                write!(formatter, "reference disagreement: {detail}")
            }
        }
    }
}

/// Verdict counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub num_passed: usize,
    pub num_accepted: usize,
    pub num_skipped: usize,
    pub num_failed: usize,
}

impl Summary {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The counts of `reports`.
    #[must_use]
    pub fn of(reports: &[CaseReport]) -> Self {
        let mut summary = Self::default();
        for report in reports {
            match report.verdict {
                Verdict::Pass => summary.num_passed += 1,
                Verdict::Accepted { .. } => summary.num_accepted += 1,
                Verdict::Skipped { .. } => summary.num_skipped += 1,
                Verdict::Fail(_) => summary.num_failed += 1,
            }
        }
        summary
    }
}

impl std::fmt::Display for Summary {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} passed, {} accepted divergences, {} skipped, {} failed",
            self.num_passed, self.num_accepted, self.num_skipped, self.num_failed
        )
    }
}

/// Runs a reader under test over the corpus.
///
/// # Type Parameters
///
/// * `ReaderType` - The reader under test.
pub struct Harness<'harness, ReaderType: ReaderUnderTest> {
    corpus: &'harness Corpus,
    registry: &'harness Registry,
    cpp: &'harness CppClpS,
    reader: &'harness ReaderType,
    work_dir: &'harness Path,
    options: HarnessOptions,
    reference_records: RefCell<HashMap<String, Rc<ReferenceRecords>>>,
    dataset_references: RefCell<HashMap<String, Rc<DatasetReference>>>,
}

impl<'harness, ReaderType: ReaderUnderTest> Harness<'harness, ReaderType> {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A harness that runs `reader` (and `cpp`, for probes and reference records) with scratch
    /// files under `work_dir`.
    #[must_use]
    pub fn new(
        corpus: &'harness Corpus,
        registry: &'harness Registry,
        cpp: &'harness CppClpS,
        reader: &'harness ReaderType,
        work_dir: &'harness Path,
        options: HarnessOptions,
    ) -> Self {
        Self {
            corpus,
            registry,
            cpp,
            reader,
            work_dir,
            options,
            reference_records: RefCell::new(HashMap::new()),
            dataset_references: RefCell::new(HashMap::new()),
        }
    }

    /// Decompresses every fixture archive and compares it with its `x` oracle; rejection
    /// fixtures must fail at open with `UnsupportedArchive`.
    ///
    /// # Returns
    ///
    /// One report per archive, in manifest order.
    #[must_use]
    pub fn run_fixtures(&self) -> Vec<CaseReport> {
        self.corpus
            .archive_oracles
            .iter()
            .filter(|oracle| {
                self.is_selected(&oracle.fixture) || self.is_selected(&oracle.archive_name())
            })
            .map(|oracle| CaseReport {
                case: oracle.archive_name(),
                output: None,
                verdict: self
                    .judge_fixture(oracle)
                    .unwrap_or_else(|error| infrastructure_failure(&error)),
            })
            .collect()
    }

    /// Runs every fixture query of the suite, for each of its outputs.
    ///
    /// # Returns
    ///
    /// One report per (query, output), in suite order.
    #[must_use]
    pub fn run_queries(&self) -> Vec<CaseReport> {
        self.run_query_cases(false)
    }

    /// Decompresses every dataset and compares it with its `x` oracle, then runs every dataset
    /// query of the suite.
    ///
    /// # Returns
    ///
    /// One report per dataset (`x:<name>`) and per dataset query, in manifest and suite order.
    #[must_use]
    pub fn run_datasets(&self) -> Vec<CaseReport> {
        let mut reports: Vec<CaseReport> = self
            .corpus
            .datasets
            .iter()
            .filter(|dataset| self.is_selected(&dataset.name))
            .map(|dataset| CaseReport {
                case: format!("x:{}", dataset.name),
                output: None,
                verdict: self
                    .judge_dataset_extract(dataset)
                    .unwrap_or_else(|error| infrastructure_failure(&error)),
            })
            .collect();
        reports.extend(self.run_query_cases(true));
        reports
    }

    /// # Returns
    ///
    /// The reports of the fixture (or dataset) query cases.
    fn run_query_cases(&self, is_dataset: bool) -> Vec<CaseReport> {
        let mut reports = Vec::new();
        for case in &self.corpus.queries {
            let target_name = match &case.target {
                QueryTarget::Fixture(name) if !is_dataset => name,
                QueryTarget::Dataset(name) if is_dataset => name,
                QueryTarget::Fixture(_) | QueryTarget::Dataset(_) => continue,
            };
            if !self.is_selected(&case.id) && !self.is_selected(target_name) {
                continue;
            }
            for output in &case.outputs {
                tracing::debug!(case = % case.id, output = % output, "Running query case.");
                reports.push(CaseReport {
                    case: case.id.clone(),
                    output: Some(*output),
                    verdict: self
                        .judge_query(case, *output)
                        .unwrap_or_else(|error| infrastructure_failure(&error)),
                });
            }
        }
        reports
    }

    /// # Returns
    ///
    /// The verdict of one fixture archive on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if a readable fixture has no oracle.
    /// * Forwards [`ReaderUnderTest::extract`]'s return values on failure.
    /// * Forwards [`read_oracle`]'s return values on failure.
    /// * Forwards [`Self::registered`]'s return values on failure.
    /// * Forwards [`Self::judge_records`]'s return values on failure.
    fn judge_fixture(&self, oracle: &ArchiveOracle) -> anyhow::Result<Verdict> {
        let work = self.case_work_dir()?;
        let outcome = self.reader.extract(&oracle.archive, work.path())?;
        if oracle.expectation == Expectation::Reject {
            let exclusion = match oracle.kind {
                FixtureKind::RejectMfa => "SX-01",
                FixtureKind::RejectSfa | FixtureKind::Sfa | FixtureKind::Kvir => "SX-02",
            };
            return Ok(match outcome.failure {
                None => Verdict::Fail(Failure::NotRejected {
                    num_records: RecordMultiset::from_jsonl(&outcome.jsonl).len(),
                }),
                Some(failure) if failure.is_unsupported_archive() && outcome.jsonl.is_empty() => {
                    Verdict::Accepted {
                        divergences: self.registered(&[exclusion])?,
                        detail: format!("rejected at open: {failure}"),
                    }
                }
                Some(failure) => Verdict::Fail(Failure::WrongRejection {
                    detail: format!("{failure} (after {} output bytes)", outcome.jsonl.len()),
                }),
            });
        }
        if let Some(failure) = outcome.failure {
            return Ok(Verdict::Fail(Failure::ReaderFailed { failure }));
        }
        let (Some(oracle_path), Some(expected_sha256)) = (&oracle.oracle, &oracle.canonical_sha256)
        else {
            anyhow::bail!(
                "readable fixture {} has no oracle",
                oracle.archive.display()
            );
        };
        if CanonicalDigest::of_jsonl(&outcome.jsonl).sha256 == *expected_sha256 {
            return Ok(Verdict::Pass);
        }
        let Some(expected) = read_oracle(oracle_path, expected_sha256)? else {
            return Ok(stale_oracle(oracle_path));
        };
        self.judge_records(&expected, &RecordMultiset::from_jsonl(&outcome.jsonl))
    }

    /// # Returns
    ///
    /// The verdict of one dataset decompression on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`ReaderUnderTest::extract`]'s return values on failure.
    /// * Forwards [`read_oracle`]'s return values on failure.
    /// * Forwards [`Self::judge_records`]'s return values on failure.
    fn judge_dataset_extract(&self, dataset: &DatasetOracle) -> anyhow::Result<Verdict> {
        let work = self.case_work_dir()?;
        let outcome = self.reader.extract(&dataset.archive, work.path())?;
        if let Some(failure) = outcome.failure {
            return Ok(Verdict::Fail(Failure::ReaderFailed { failure }));
        }
        let digest = CanonicalDigest::of_jsonl(&outcome.jsonl);
        if digest.sha256 == dataset.x_canonical_sha256 && digest.count == dataset.x_records {
            return Ok(Verdict::Pass);
        }
        let Some(expected) = read_oracle(&dataset.x_oracle, &dataset.x_canonical_sha256)? else {
            return Ok(stale_oracle(&dataset.x_oracle));
        };
        self.judge_records(&expected, &RecordMultiset::from_jsonl(&outcome.jsonl))
    }

    /// # Returns
    ///
    /// The verdict of one (query, output) on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::search`]'s return values on failure.
    /// * Forwards [`Self::judge_stdout`]'s return values on failure.
    /// * Forwards [`Self::judge_msgpack`]'s return values on failure.
    /// * Forwards [`Self::judge_results_cache`]'s return values on failure.
    fn judge_query(&self, case: &QueryCase, output: OutputKind) -> anyhow::Result<Verdict> {
        let Some(oracle) = self.corpus.query_oracle(&case.id, output) else {
            return Ok(invalid_case(format!("no C++ oracle for output {output}")));
        };
        let declared = match self.registry.resolve_declared(&case.divergences) {
            Ok(declared) => declared,
            Err(error) => return Ok(invalid_case(error.to_string())),
        };
        let outcome = self.search(case, oracle, output)?;
        match outcome.output {
            SearchOutput::JsonLines(jsonl) => {
                self.judge_stdout(case, oracle, &declared, outcome.failure, &jsonl)
            }
            SearchOutput::Msgpack(tuples) => {
                self.judge_msgpack(case, oracle, &declared, outcome.failure, &tuples)
            }
            SearchOutput::ResultsCache(documents) => {
                self.judge_results_cache(case, oracle, &declared, outcome.failure, &documents)
            }
            SearchOutput::Unsupported => Ok(Verdict::Skipped {
                reason: format!("{} does not implement output {output}", self.reader.name()),
            }),
        }
    }

    /// # Returns
    ///
    /// The verdict of a stdout output on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`read_oracle`]'s return values on failure.
    /// * Forwards [`Self::judge_records`]'s return values on failure.
    /// * Forwards [`Self::judge_declared_stdout`]'s return values on failure.
    fn judge_stdout(
        &self,
        case: &QueryCase,
        oracle: &QueryOracle,
        declared: &[&Divergence],
        failure: Option<ReaderFailure>,
        jsonl: &[u8],
    ) -> anyhow::Result<Verdict> {
        if let Some(expected_exit) = case.expected_rust_exit {
            let actual_exit = failure
                .as_ref()
                .map_or(0, |failure| failure.exit_code.unwrap_or(1));
            let num_records = RecordMultiset::from_jsonl(jsonl).len();
            if actual_exit != expected_exit || num_records != 0 {
                return Ok(Verdict::Fail(Failure::ExpectationMismatch {
                    detail: format!(
                        "expected exit {expected_exit} and no records, got exit {actual_exit} and \
                         {num_records} records"
                    ),
                }));
            }
            if oracle.cpp_exit == expected_exit {
                return Ok(not_reproduced(declared));
            }
            return Ok(accepted(
                declared,
                format!("exit {actual_exit} (C++ exits {})", oracle.cpp_exit),
            ));
        }
        if let Some(failure) = failure {
            return Ok(Verdict::Fail(Failure::ReaderFailed { failure }));
        }
        let digest = CanonicalDigest::of_jsonl(jsonl);
        let is_cpp_equal = oracle.canonical_sha256.as_deref() == Some(digest.sha256.as_str())
            && digest.count == oracle.count;
        if !declared.is_empty() {
            return self.judge_declared_stdout(
                case,
                oracle,
                declared,
                jsonl,
                &digest,
                is_cpp_equal,
            );
        }
        if is_cpp_equal {
            return self.with_reference_check(case, digest.count, jsonl, Verdict::Pass);
        }
        let Some(expected_sha256) = &oracle.canonical_sha256 else {
            return Ok(invalid_case(
                "the stdout oracle has no canonical digest".to_owned(),
            ));
        };
        let Some(expected) = read_oracle(&oracle.file, expected_sha256)? else {
            return Ok(stale_oracle(&oracle.file));
        };
        let actual = RecordMultiset::from_jsonl(jsonl);
        let verdict = self.judge_records(&expected, &actual)?;
        if !verdict.is_failure() {
            return self.with_reference_check(case, digest.count, jsonl, verdict);
        }
        Ok(self.with_probe_findings(case, &expected, &actual, verdict))
    }

    /// Checks a case whose output agrees with C++ against the third oracle, `clp_s_query`'s
    /// reference evaluator (if enabled).
    ///
    /// # Returns
    ///
    /// `verdict`, or the reference disagreement, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::check_reference_records`]'s return values on failure.
    fn with_reference_check(
        &self,
        case: &QueryCase,
        num_records: u64,
        jsonl: &[u8],
        verdict: Verdict,
    ) -> anyhow::Result<Verdict> {
        Ok(self
            .check_reference_records(case, num_records, jsonl)?
            .map_or(verdict, Verdict::Fail))
    }

    /// # Returns
    ///
    /// The verdict of a stdout output of a case that declares divergences on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::judge_records_by_id`]'s return values on failure.
    /// * Forwards [`Self::check_reference`]'s return values on failure.
    /// * Forwards [`Self::check_reference_records`]'s return values on failure.
    fn judge_declared_stdout(
        &self,
        case: &QueryCase,
        oracle: &QueryOracle,
        declared: &[&Divergence],
        jsonl: &[u8],
        digest: &CanonicalDigest,
        is_cpp_equal: bool,
    ) -> anyhow::Result<Verdict> {
        if is_cpp_equal {
            return Ok(not_reproduced(declared));
        }
        if let Some(expected_ids) = &case.expected_rust_ids {
            let id_key = case.id_key();
            let actual_records = records_by_id(jsonl, id_key);
            let mut actual_ids: Vec<RecordKey> =
                actual_records.iter().map(|(id, _)| id.clone()).collect();
            actual_ids.sort();
            let mut expected_ids = expected_ids.clone();
            expected_ids.sort();
            if actual_ids != expected_ids {
                return Ok(Verdict::Fail(Failure::ExpectationMismatch {
                    detail: id_difference(&expected_ids, &actual_ids),
                }));
            }
            let by_id = self.judge_records_by_id(case, oracle, id_key, &actual_records)?;
            if by_id.is_failure() {
                return Ok(by_id);
            }
            if let Some(disagreement) = self.check_reference(case, oracle, &expected_ids)? {
                return Ok(Verdict::Fail(disagreement));
            }
            return Ok(accepted(
                declared,
                format!(
                    "{} records as expected (C++ returns {})",
                    actual_ids.len(),
                    oracle.count
                ),
            ));
        }
        if let Some(expected_count) = case.expected_rust_count {
            if digest.count != expected_count {
                return Ok(Verdict::Fail(Failure::ExpectationMismatch {
                    detail: format!("expected {expected_count} records, got {}", digest.count),
                }));
            }
            if let Some(failure) = self.check_reference_records(case, expected_count, jsonl)? {
                return Ok(Verdict::Fail(failure));
            }
            if oracle.count == expected_count {
                return Ok(not_reproduced(declared));
            }
            return Ok(accepted(
                declared,
                format!(
                    "{expected_count} records as expected (C++ returns {})",
                    oracle.count
                ),
            ));
        }
        Ok(invalid_case(
            "the case declares a divergence but states no expected Rust result".to_owned(),
        ))
    }

    /// Compares the reader's records with the C++ records of the same ids as canonical
    /// multisets: a record C++ also returns with the C++ search output, and a record only the
    /// reader returns with the fixture's `x` oracle. Without `x` records (a dataset target or a
    /// projection), only the records both return are compared.
    ///
    /// # Returns
    ///
    /// [`Verdict::Pass`] if they agree (up to float normalization), else the failure, on
    /// success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`read_verified_oracle`]'s return values on failure.
    /// * Forwards [`read_file`]'s return values on failure.
    /// * Forwards [`Self::decompressed_records`]'s return values on failure.
    /// * Forwards [`Self::judge_records`]'s return values on failure.
    fn judge_records_by_id(
        &self,
        case: &QueryCase,
        oracle: &QueryOracle,
        id_key: &str,
        actual_records: &[(RecordKey, Vec<u8>)],
    ) -> anyhow::Result<Verdict> {
        let oracle_data = match &oracle.canonical_sha256 {
            Some(expected_sha256) => {
                let Some(data) = read_verified_oracle(&oracle.file, expected_sha256)? else {
                    return Ok(stale_oracle(&oracle.file));
                };
                data
            }
            None => read_file(&oracle.file)?,
        };
        let oracle_records = records_by_id(&oracle_data, id_key);
        let oracle_ids: HashSet<&RecordKey> = oracle_records.iter().map(|(id, _)| id).collect();
        let actual_ids: HashSet<&RecordKey> = actual_records.iter().map(|(id, _)| id).collect();
        let mut expected = RecordMultiset::default();
        for (id, record) in &oracle_records {
            if actual_ids.contains(id) {
                expected.insert(canonical_record(record));
            }
        }
        let reader_only_ids: HashSet<&RecordKey> = actual_ids
            .iter()
            .copied()
            .filter(|id| !oracle_ids.contains(id))
            .collect();
        let decompressed = if reader_only_ids.is_empty() {
            None
        } else {
            self.decompressed_records(case, id_key, &reader_only_ids)?
        };
        for record in decompressed.iter().flatten() {
            expected.insert(record.clone());
        }
        let mut actual = RecordMultiset::default();
        for (id, record) in actual_records {
            if decompressed.is_some() || oracle_ids.contains(id) {
                actual.insert(canonical_record(record));
            }
        }
        self.judge_records(&expected, &actual)
    }

    /// # Returns
    ///
    /// The canonical records with an id in `ids` of the `x` oracles of the case's fixture, or
    /// `None` if the case targets a dataset or projects columns, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if the fixture has no `x` oracle.
    /// * Forwards [`read_file`]'s return values on failure.
    fn decompressed_records(
        &self,
        case: &QueryCase,
        id_key: &str,
        ids: &HashSet<&RecordKey>,
    ) -> anyhow::Result<Option<Vec<Vec<u8>>>> {
        let QueryTarget::Fixture(fixture_name) = &case.target else {
            return Ok(None);
        };
        if !case.options.projection.is_empty() {
            return Ok(None);
        }
        let mut records = Vec::new();
        let mut num_oracles = 0;
        for archive in &self.corpus.archive_oracles {
            if archive.fixture != *fixture_name {
                continue;
            }
            if let Some(path) = &archive.oracle {
                for (id, record) in records_by_id(&read_file(path)?, id_key) {
                    if ids.contains(&id) {
                        records.push(canonical_record(&record));
                    }
                }
                num_oracles += 1;
            }
        }
        anyhow::ensure!(num_oracles > 0, "fixture {fixture_name} has no x oracle");
        Ok(Some(records))
    }

    /// Checks the case's expected ids against `scripts/reference_eval.py` (recorded in the
    /// oracle manifest) and, if enabled, against `clp_s_query`'s reference evaluator.
    ///
    /// # Returns
    ///
    /// The disagreement, if any, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::reference_records`]'s return values on failure.
    /// * Forwards [`ReferenceRecords::matching_ids`]'s return values on failure.
    fn check_reference(
        &self,
        case: &QueryCase,
        oracle: &QueryOracle,
        expected_ids: &[RecordKey],
    ) -> anyhow::Result<Option<Failure>> {
        if let Some(reference_ids) = &oracle.reference_ids {
            let mut reference_ids = reference_ids.clone();
            reference_ids.sort();
            if reference_ids != expected_ids {
                return Ok(Some(Failure::ReferenceDisagreement {
                    detail: format!(
                        "scripts/reference_eval.py: {}",
                        id_difference(expected_ids, &reference_ids)
                    ),
                }));
            }
        }
        let Some(records) = self.reference_records(case)? else {
            return Ok(None);
        };
        let reference_ids = records.matching_ids(case, case.id_key())?;
        if reference_ids != expected_ids {
            return Ok(Some(Failure::ReferenceDisagreement {
                detail: format!(
                    "clp_s_query reference evaluator: {}",
                    id_difference(expected_ids, &reference_ids)
                ),
            }));
        }
        Ok(None)
    }

    /// Checks a case against `clp_s_query`'s reference evaluator, if enabled: it must match
    /// `expected_count` records, and (without a projection) exactly the reader's records.
    ///
    /// # Returns
    ///
    /// The disagreement, if any, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::check_dataset_reference`]'s return values on failure.
    /// * Forwards [`Self::reference_records`]'s return values on failure.
    /// * Forwards [`ReferenceRecords::matching_records`]'s return values on failure.
    /// * Forwards [`Self::judge_records`]'s return values on failure.
    fn check_reference_records(
        &self,
        case: &QueryCase,
        expected_count: u64,
        jsonl: &[u8],
    ) -> anyhow::Result<Option<Failure>> {
        if let QueryTarget::Dataset(name) = &case.target {
            return self.check_dataset_reference(case, name, expected_count, jsonl);
        }
        let Some(records) = self.reference_records(case)? else {
            return Ok(None);
        };
        let matching = records.matching_records(case)?;
        if matching.len() as u64 != expected_count {
            return Ok(Some(Failure::ReferenceDisagreement {
                detail: format!(
                    "clp_s_query reference evaluator: expected {expected_count} records, it \
                     matches {}",
                    matching.len()
                ),
            }));
        }
        if !case.options.projection.is_empty() {
            return Ok(None);
        }
        let mut expected = RecordMultiset::default();
        for record in matching {
            expected.insert(canonical_record(&record.message));
        }
        Ok(
            match self.judge_records(&expected, &RecordMultiset::from_jsonl(jsonl))? {
                Verdict::Fail(Failure::RecordMismatch { report }) => {
                    Some(Failure::ReferenceDisagreement {
                        detail: format!(
                            "the records differ from the clp_s_query reference evaluator's \
                             matches: {report}"
                        ),
                    })
                }
                Verdict::Fail(failure) => Some(failure),
                Verdict::Pass | Verdict::Accepted { .. } | Verdict::Skipped { .. } => None,
            },
        )
    }

    /// Checks a dataset case against `clp_s_query`'s reference evaluator over the dataset's `x`
    /// records, if enabled: it must match `expected_count` records, and (without a projection)
    /// records whose multiset fingerprint equals the reader's.
    ///
    /// # Returns
    ///
    /// The disagreement, if any, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if the case targets an unknown dataset, or the reference did not
    ///   evaluate it.
    /// * Forwards [`DatasetReference::evaluate`]'s return values on failure.
    fn check_dataset_reference(
        &self,
        case: &QueryCase,
        dataset_name: &str,
        expected_count: u64,
        jsonl: &[u8],
    ) -> anyhow::Result<Option<Failure>> {
        if !self.options.is_reference_evaluator_enabled {
            return Ok(None);
        }
        let cached = self.dataset_references.borrow().get(dataset_name).cloned();
        let reference = if let Some(reference) = cached {
            reference
        } else {
            let dataset = self
                .corpus
                .dataset(dataset_name)
                .ok_or_else(|| anyhow::anyhow!("unknown dataset {dataset_name}"))?;
            let target = QueryTarget::Dataset(dataset_name.to_owned());
            let cases: Vec<&QueryCase> = self
                .corpus
                .queries
                .iter()
                .filter(|query| query.target == target)
                .collect();
            tracing::info!(
                dataset = % dataset_name,
                num_queries = cases.len(),
                "Evaluating the dataset queries with the reference evaluator."
            );
            let reference = Rc::new(DatasetReference::evaluate(dataset, &cases)?);
            self.dataset_references
                .borrow_mut()
                .insert(dataset_name.to_owned(), Rc::clone(&reference));
            reference
        };
        let expected = *reference.result(&case.id).ok_or_else(|| {
            anyhow::anyhow!("the reference evaluator did not evaluate {}", case.id)
        })?;
        if expected.count != expected_count {
            return Ok(Some(Failure::ReferenceDisagreement {
                detail: format!(
                    "clp_s_query reference evaluator: expected {expected_count} records, it \
                     matches {}",
                    expected.count
                ),
            }));
        }
        if case.options.projection.is_empty() && MultisetFingerprint::of_jsonl(jsonl) != expected {
            return Ok(Some(Failure::ReferenceDisagreement {
                detail: format!(
                    "the {expected_count} records differ from the clp_s_query reference \
                     evaluator's matches over the `x` records"
                ),
            }));
        }
        Ok(None)
    }

    /// Loads the records of the case's fixture for `clp_s_query`'s reference evaluator, once per
    /// fixture.
    ///
    /// # Returns
    ///
    /// The records, or `None` if the reference evaluator is disabled or the case targets a
    /// dataset, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if the case targets an unknown fixture.
    /// * Forwards [`Self::case_work_dir`]'s return values on failure.
    /// * Forwards [`ReferenceRecords::load`]'s return values on failure.
    fn reference_records(&self, case: &QueryCase) -> anyhow::Result<Option<Rc<ReferenceRecords>>> {
        if !self.options.is_reference_evaluator_enabled {
            return Ok(None);
        }
        let QueryTarget::Fixture(fixture_name) = &case.target else {
            return Ok(None);
        };
        if let Some(records) = self.reference_records.borrow().get(fixture_name) {
            return Ok(Some(Rc::clone(records)));
        }
        let fixture = self
            .corpus
            .fixture(fixture_name)
            .ok_or_else(|| anyhow::anyhow!("unknown fixture {fixture_name}"))?;
        let work = self.case_work_dir()?;
        let records = Rc::new(ReferenceRecords::load(self.cpp, fixture, work.path())?);
        self.reference_records
            .borrow_mut()
            .insert(fixture_name.clone(), Rc::clone(&records));
        Ok(Some(records))
    }

    /// # Returns
    ///
    /// The verdict of a msgpack output on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`read_file`]'s return values on failure.
    /// * Forwards [`WireTuple::decode_stream`]'s return values on failure.
    /// * Forwards [`Self::search`]'s return values on failure.
    fn judge_msgpack(
        &self,
        case: &QueryCase,
        oracle: &QueryOracle,
        declared: &[&Divergence],
        failure: Option<ReaderFailure>,
        tuples: &[WireTuple],
    ) -> anyhow::Result<Verdict> {
        if let Some(failure) = failure {
            return Ok(Verdict::Fail(Failure::ReaderFailed { failure }));
        }
        let actual = tuple_multiset(tuples);
        if declared.is_empty() {
            let expected = tuple_multiset(&WireTuple::decode_stream(&read_file(&oracle.file)?)?);
            return Ok(multiset_verdict(
                &expected,
                &actual,
                self.options.max_diff_records,
            ));
        }
        if !is_declared_only(declared, &["DV-21"]) {
            return Ok(invalid_case(
                "a msgpack case may only declare DV-21 (per-session output file)".to_owned(),
            ));
        }
        let stdout = self.search(case, oracle, OutputKind::Stdout)?;
        let SearchOutput::JsonLines(jsonl) = stdout.output else {
            return Ok(Verdict::Skipped {
                reason: "DV-21 needs the reader's stdout output".to_owned(),
            });
        };
        let mut messages = RecordMultiset::default();
        for tuple in tuples {
            messages.insert(tuple.canonical().message);
        }
        let stdout_records = RecordMultiset::from_jsonl(&jsonl);
        if stdout_records != messages {
            return Ok(Verdict::Fail(Failure::ExpectationMismatch {
                detail: format!(
                    "the tuples must hold every match of the session (the stdout records): {}",
                    stdout_records
                        .diff(&messages)
                        .render(self.options.max_diff_records)
                ),
            }));
        }
        if oracle.count == messages.len() {
            return Ok(not_reproduced(declared));
        }
        Ok(accepted(
            declared,
            format!("{} tuples, C++ kept {}", messages.len(), oracle.count),
        ))
    }

    /// # Returns
    ///
    /// The verdict of a results-cache output on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if the oracle is not UTF-8.
    /// * Forwards [`read_file`]'s return values on failure.
    /// * Forwards [`ResultsCacheDocument::parse_dump`]'s return values on failure.
    /// * Forwards [`Self::search`]'s return values on failure.
    fn judge_results_cache(
        &self,
        case: &QueryCase,
        oracle: &QueryOracle,
        declared: &[&Divergence],
        failure: Option<ReaderFailure>,
        documents: &[ResultsCacheDocument],
    ) -> anyhow::Result<Verdict> {
        if let Some(failure) = failure {
            return Ok(Verdict::Fail(Failure::ReaderFailed { failure }));
        }
        let actual = document_multiset(documents);
        let oracle_text = String::from_utf8(read_file(&oracle.file)?)?;
        let expected = document_multiset(&ResultsCacheDocument::parse_dump(&oracle_text)?);
        if declared.is_empty() {
            return Ok(multiset_verdict(
                &expected,
                &actual,
                self.options.max_diff_records,
            ));
        }
        if !is_declared_only(declared, &["DV-24", "DV-25"]) {
            return Ok(invalid_case(
                "a results-cache case may only declare DV-24 or DV-25".to_owned(),
            ));
        }
        let matches = self.search(case, oracle, OutputKind::Msgpack)?;
        let SearchOutput::Msgpack(tuples) = matches.output else {
            return Ok(Verdict::Skipped {
                reason: "DV-24/DV-25 need the reader's msgpack output".to_owned(),
            });
        };
        if let Some(detail) = find_top_k_violation(documents, &tuples, case.max_num_results) {
            return Ok(Verdict::Fail(Failure::ExpectationMismatch { detail }));
        }
        if expected == actual {
            return Ok(not_reproduced(declared));
        }
        Ok(accepted(
            declared,
            format!(
                "session-wide top-{}: {} documents (C++ inserted {})",
                case.max_num_results,
                documents.len(),
                oracle.count
            ),
        ))
    }

    /// Runs the reader on a case's archives with the given output.
    ///
    /// # Returns
    ///
    /// The reader's outcome on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`ReaderUnderTest::search`]'s return values on failure.
    fn search(
        &self,
        case: &QueryCase,
        oracle: &QueryOracle,
        output: OutputKind,
    ) -> anyhow::Result<SearchOutcome> {
        let work = self.case_work_dir()?;
        let request = SearchRequest {
            archives: &oracle.archives,
            kql: &case.kql,
            options: &case.options,
            output: match output {
                OutputKind::Stdout => OutputRequest::Stdout,
                OutputKind::Msgpack => OutputRequest::Msgpack,
                OutputKind::ResultsCache => OutputRequest::ResultsCache {
                    max_num_results: case.max_num_results,
                    dataset: case.dataset_name.clone(),
                },
            },
        };
        self.reader.search(&request, work.path())
    }

    /// Compares records exactly, then under the DV-01/DV-02 float normalization.
    ///
    /// # Returns
    ///
    /// [`Verdict::Pass`], [`Verdict::Accepted`] (DV-01/DV-02) or a record mismatch on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::registered`]'s return values on failure.
    fn judge_records(
        &self,
        expected: &RecordMultiset,
        actual: &RecordMultiset,
    ) -> anyhow::Result<Verdict> {
        let comparison = RecordComparison::compare(expected, actual);
        if comparison.is_exact() {
            return Ok(Verdict::Pass);
        }
        if !comparison.is_explained() {
            return Ok(Verdict::Fail(Failure::RecordMismatch {
                report: comparison.render(self.options.max_diff_records),
            }));
        }
        let mut ids = Vec::new();
        if comparison.uses_rounding() {
            ids.push("DV-01");
        }
        if comparison.uses_non_finite() {
            ids.push("DV-02");
        }
        Ok(Verdict::Accepted {
            divergences: self.registered(&ids)?,
            detail: format!(
                "{} record pairs equal under float normalization",
                comparison.accepted.len()
            ),
        })
    }

    /// Appends the archive-dependence probe's findings to a record-mismatch verdict of a
    /// fixture case, if the probe is enabled.
    ///
    /// # Returns
    ///
    /// The verdict, with the findings in its report.
    fn with_probe_findings(
        &self,
        case: &QueryCase,
        expected: &RecordMultiset,
        actual: &RecordMultiset,
        verdict: Verdict,
    ) -> Verdict {
        let Verdict::Fail(Failure::RecordMismatch { mut report }) = verdict else {
            return verdict;
        };
        let QueryTarget::Fixture(fixture_name) = &case.target else {
            return Verdict::Fail(Failure::RecordMismatch { report });
        };
        let Some(fixture) = self
            .corpus
            .fixture(fixture_name)
            .filter(|_| self.options.is_archive_dependence_probe_enabled)
        else {
            return Verdict::Fail(Failure::RecordMismatch { report });
        };
        let findings = self.case_work_dir().and_then(|work| {
            probe_archive_dependence(
                self.cpp,
                self.registry,
                fixture,
                case,
                &expected.diff(actual),
                work.path(),
                self.options.max_probed_records,
            )
        });
        let lines: Vec<String> = match findings {
            Ok(findings) => findings
                .iter()
                .map(|finding| {
                    format!(
                        "probe: rust_match={} cpp_single_record_match={} archive_dependent={} \
                         suggestion={}",
                        finding.is_rust_match,
                        finding.is_cpp_single_match,
                        finding.is_archive_dependent(),
                        finding
                            .suggestion
                            .as_ref()
                            .map_or("none", DivergenceId::as_str)
                    )
                })
                .collect(),
            Err(error) => vec![format!("probe failed: {error:#}")],
        };
        for line in lines {
            report.push('\n');
            report.push_str(&line);
        }
        Verdict::Fail(Failure::RecordMismatch { report })
    }

    /// # Returns
    ///
    /// The registered ids of `ids` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if an id is not registered.
    fn registered(&self, ids: &[&str]) -> anyhow::Result<Vec<DivergenceId>> {
        ids.iter()
            .map(|id| {
                self.registry
                    .get(id)
                    .map(|entry| entry.id.clone())
                    .ok_or_else(|| anyhow::anyhow!("divergence {id} is not registered"))
            })
            .collect()
    }

    /// # Returns
    ///
    /// Whether the case filter selects `name`.
    fn is_selected(&self, name: &str) -> bool {
        self.options.case_filter.is_empty()
            || self
                .options
                .case_filter
                .iter()
                .any(|selected| selected == name)
    }

    /// # Returns
    ///
    /// A fresh scratch directory for one case on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if the directory cannot be created.
    fn case_work_dir(&self) -> anyhow::Result<tempfile::TempDir> {
        std::fs::create_dir_all(self.work_dir)?;
        Ok(tempfile::Builder::new()
            .prefix("case-")
            .tempdir_in(self.work_dir)?)
    }
}

/// Finds why results-cache documents are not the session-wide top-k of all matches (DV-24,
/// DV-25).
///
/// A valid top-k has `min(k, matches)` documents with distinct `_id`s, every one a match, none
/// older than the k-th latest match, and every match newer than it present. Ties at the k-th
/// timestamp may resolve either way.
///
/// # Returns
///
/// The first violated condition, or `None` if the documents are a valid top-k.
#[must_use]
pub fn find_top_k_violation(
    documents: &[ResultsCacheDocument],
    matches: &[WireTuple],
    max_num_results: u64,
) -> Option<String> {
    let expected_count = usize::try_from(max_num_results)
        .unwrap_or(usize::MAX)
        .min(matches.len());
    if documents.len() != expected_count {
        return Some(format!(
            "expected {expected_count} documents (top-{max_num_results} of {} matches), got {}",
            matches.len(),
            documents.len()
        ));
    }
    let ids: HashSet<(&str, i64)> = documents
        .iter()
        .map(|document| (document.archive_id.as_str(), document.log_event_idx))
        .collect();
    if ids.len() != documents.len() {
        return Some("document `_id`s collide".to_owned());
    }
    let mut timestamps: Vec<i64> = matches.iter().map(|tuple| tuple.timestamp_ms).collect();
    timestamps.sort_unstable_by(|left, right| right.cmp(left));
    let cutoff = timestamps[expected_count.checked_sub(1)?];
    let mut available: HashMap<(i64, Vec<u8>), u64> = HashMap::new();
    for tuple in matches {
        *available
            .entry((tuple.timestamp_ms, tuple.canonical().message))
            .or_default() += 1;
    }
    let mut num_newer = 0;
    for document in documents {
        let canonical = document.canonical();
        let key = (canonical.timestamp, canonical.message.into_bytes());
        match available.get_mut(&key) {
            Some(count) if *count > 0 => *count -= 1,
            _ => {
                return Some(format!(
                    "document {}:{} is not a match of the session",
                    document.archive_id, document.log_event_idx
                ));
            }
        }
        if document.timestamp < cutoff {
            return Some(format!(
                "document {}:{} (timestamp {}) is older than the k-th latest match ({cutoff})",
                document.archive_id, document.log_event_idx, document.timestamp
            ));
        }
        if document.timestamp > cutoff {
            num_newer += 1;
        }
    }
    let expected_newer = timestamps
        .iter()
        .filter(|timestamp| **timestamp > cutoff)
        .count();
    if num_newer != expected_newer {
        return Some(format!(
            "{expected_newer} matches are newer than the k-th latest ({cutoff}), but only \
             {num_newer} documents are"
        ));
    }
    None
}

/// # Returns
///
/// The `(id, raw line)` pairs of the non-empty lines of `jsonl`; a line without an integer or
/// string `id_key` member gets the id `"<no id>"`.
#[must_use]
pub fn records_by_id(jsonl: &[u8], id_key: &str) -> Vec<(RecordKey, Vec<u8>)> {
    jsonl
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            let id = JsonValue::parse(line)
                .and_then(|value| value.member(id_key.as_bytes()).and_then(record_key))
                .unwrap_or_else(|| RecordKey::Text("<no id>".to_owned()));
            (id, line.to_vec())
        })
        .collect()
}

/// # Returns
///
/// The record key of an id value, if it is an integer or a string.
fn record_key(value: &JsonValue) -> Option<RecordKey> {
    match value {
        JsonValue::Number(token) => std::str::from_utf8(token)
            .ok()?
            .parse()
            .ok()
            .map(RecordKey::Integer),
        JsonValue::String(text) => {
            Some(RecordKey::Text(String::from_utf8_lossy(text).into_owned()))
        }
        _ => None,
    }
}

/// # Returns
///
/// The ids only `expected` has and the ids only `actual` has, rendered.
fn id_difference(expected: &[RecordKey], actual: &[RecordKey]) -> String {
    let mut missing: Vec<String> = Vec::new();
    let mut unexpected: Vec<String> = Vec::new();
    let mut remaining: HashMap<&RecordKey, i64> = HashMap::new();
    for id in expected {
        *remaining.entry(id).or_default() += 1;
    }
    for id in actual {
        *remaining.entry(id).or_default() -= 1;
    }
    let mut sorted: Vec<_> = remaining.into_iter().collect();
    sorted.sort();
    for (id, count) in sorted {
        for _ in 0..count.max(0) {
            missing.push(id.to_string());
        }
        for _ in 0..(-count).max(0) {
            unexpected.push(id.to_string());
        }
    }
    format!(
        "expected {} ids, got {}; missing [{}], unexpected [{}]",
        expected.len(),
        actual.len(),
        missing.join(", "),
        unexpected.join(", ")
    )
}

/// # Returns
///
/// The multiset of canonical tuples, each rendered as one comparable line.
fn tuple_multiset(tuples: &[WireTuple]) -> RecordMultiset {
    let mut multiset = RecordMultiset::default();
    for tuple in tuples {
        let canonical = tuple.canonical();
        let mut line = format!(
            "ts={} archive_id={} log_event_idx={} orig_file_path={} message=",
            canonical.timestamp_ms,
            String::from_utf8_lossy(&canonical.archive_id),
            canonical.log_event_idx,
            String::from_utf8_lossy(&canonical.orig_file_path)
        )
        .into_bytes();
        line.extend_from_slice(&canonical.message);
        multiset.insert(line);
    }
    multiset
}

/// # Returns
///
/// The multiset of canonical documents, each rendered as one comparable line.
fn document_multiset(documents: &[ResultsCacheDocument]) -> RecordMultiset {
    let mut multiset = RecordMultiset::default();
    for document in documents {
        let canonical = document.canonical();
        multiset.insert(
            format!(
                "_id={}:{} timestamp={} dataset={} orig_file_path={} message={}",
                canonical.archive_id,
                canonical.log_event_idx,
                canonical.timestamp,
                canonical.dataset,
                canonical.orig_file_path,
                canonical.message
            )
            .into_bytes(),
        );
    }
    multiset
}

/// # Returns
///
/// [`Verdict::Pass`] if the multisets are equal, else a record mismatch.
fn multiset_verdict(
    expected: &RecordMultiset,
    actual: &RecordMultiset,
    max_diff_records: usize,
) -> Verdict {
    let diff = expected.diff(actual);
    if diff.is_empty() {
        return Verdict::Pass;
    }
    Verdict::Fail(Failure::RecordMismatch {
        report: diff.render(max_diff_records),
    })
}

/// # Returns
///
/// Whether every declared divergence is one of `allowed`.
fn is_declared_only(declared: &[&Divergence], allowed: &[&str]) -> bool {
    declared.iter().all(|entry| {
        allowed.contains(&entry.id.as_str()) && entry.classifier == Classifier::OutputShape
    })
}

/// # Returns
///
/// The acceptance of the declared divergences.
fn accepted(declared: &[&Divergence], detail: String) -> Verdict {
    Verdict::Accepted {
        divergences: declared.iter().map(|entry| entry.id.clone()).collect(),
        detail,
    }
}

/// # Returns
///
/// The failure of a declared divergence that no longer occurs.
fn not_reproduced(declared: &[&Divergence]) -> Verdict {
    Verdict::Fail(Failure::DivergenceNotReproduced {
        divergences: declared.iter().map(|entry| entry.id.clone()).collect(),
    })
}

/// # Returns
///
/// An invalid-case failure.
const fn invalid_case(detail: String) -> Verdict {
    Verdict::Fail(Failure::InvalidCase { detail })
}

/// # Returns
///
/// An infrastructure failure with the error's full chain.
fn infrastructure_failure(error: &anyhow::Error) -> Verdict {
    Verdict::Fail(Failure::Infrastructure {
        error: format!("{error:#}"),
    })
}

/// Reads a JSON-lines oracle, checking it against the canonical digest its manifest records.
///
/// # Returns
///
/// The oracle's records, or `None` if their digest differs from `expected_sha256` (the oracle file
/// is stale, or the canonicalizer disagrees with `scripts/corpus.py`), on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`read_verified_oracle`]'s return values on failure.
fn read_oracle(path: &Path, expected_sha256: &str) -> anyhow::Result<Option<RecordMultiset>> {
    Ok(read_verified_oracle(path, expected_sha256)?.map(|data| RecordMultiset::from_jsonl(&data)))
}

/// Reads a JSON-lines oracle, checking it against the canonical digest its manifest records.
///
/// # Returns
///
/// The oracle's bytes, or `None` if their canonical digest differs from `expected_sha256`, on
/// success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`read_file`]'s return values on failure.
fn read_verified_oracle(path: &Path, expected_sha256: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let data = read_file(path)?;
    if CanonicalDigest::of_jsonl(&data).sha256 != expected_sha256 {
        return Ok(None);
    }
    Ok(Some(data))
}

/// # Returns
///
/// The infrastructure failure of an oracle whose digest differs from its manifest.
fn stale_oracle(path: &Path) -> Verdict {
    Verdict::Fail(Failure::Infrastructure {
        error: format!(
            "the canonical digest of {} differs from its manifest: the oracle is stale or the \
             canonicalizer disagrees with scripts/corpus.py",
            path.display()
        ),
    })
}

/// # Returns
///
/// The bytes of `path` on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if the file cannot be read.
fn read_file(path: &Path) -> anyhow::Result<Vec<u8>> {
    std::fs::read(path).map_err(|error| anyhow::anyhow!("cannot read {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::path::PathBuf;

    use clp_s_reader::ErrorKind;

    use super::CaseReport;
    use super::Failure;
    use super::Harness;
    use super::HarnessOptions;
    use super::Summary;
    use super::Verdict;
    use super::find_top_k_violation;
    use crate::canonical::canonical_digest;
    use crate::corpus::ArchiveOracle;
    use crate::corpus::Corpus;
    use crate::corpus::Expectation;
    use crate::corpus::FixtureKind;
    use crate::corpus::OutputKind;
    use crate::corpus::QueryCase;
    use crate::corpus::QueryOracle;
    use crate::corpus::QueryTarget;
    use crate::corpus::RecordKey;
    use crate::corpus::prototype_root;
    use crate::cpp::CppClpS;
    use crate::reader::ExtractOutcome;
    use crate::reader::ReaderFailure;
    use crate::reader::ReaderUnderTest;
    use crate::reader::ResultsCacheDocument;
    use crate::reader::SearchOptions;
    use crate::reader::SearchOutcome;
    use crate::reader::SearchOutput;
    use crate::reader::SearchRequest;
    use crate::reader::WireTuple;
    use crate::registry::DivergenceId;
    use crate::registry::Registry;

    const BASE_CASE: QueryCase = QueryCase {
        id: String::new(),
        target: QueryTarget::Fixture(String::new()),
        kql: String::new(),
        options: SearchOptions {
            time_begin_ms: None,
            time_end_ms: None,
            is_case_insensitive: false,
            projection: Vec::new(),
        },
        outputs: Vec::new(),
        max_num_results: 10,
        dataset_name: None,
        divergences: Vec::new(),
        expected_rust_ids: None,
        expected_rust_count: None,
        expected_rust_exit: None,
    };

    const SEARCH_RECORDS: &[u8] = b"{\"rid\":\"r1\",\"v\":1}\n{\"rid\":\"r2\",\"v\":2}\n";

    /// A reader that replays canned outcomes: extracts by archive path, searches by KQL text and
    /// output kind.
    #[derive(Default)]
    struct FakeReader {
        extracts: HashMap<PathBuf, ExtractOutcome>,
        searches: HashMap<(String, OutputKind), SearchOutcome>,
    }

    impl FakeReader {
        /// # Returns
        ///
        /// The reader, which now answers searches of `kql` for `output` with `outcome`.
        fn with_search(mut self, kql: &str, output: OutputKind, outcome: SearchOutput) -> Self {
            self.searches.insert(
                (kql.to_owned(), output),
                SearchOutcome {
                    output: outcome,
                    failure: None,
                },
            );
            self
        }
    }

    impl ReaderUnderTest for FakeReader {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn extract(&self, archive: &Path, _work_dir: &Path) -> anyhow::Result<ExtractOutcome> {
            self.extracts
                .get(archive)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no canned extract for {}", archive.display()))
        }

        fn search(
            &self,
            request: &SearchRequest<'_>,
            _work_dir: &Path,
        ) -> anyhow::Result<SearchOutcome> {
            self.searches
                .get(&(request.kql.to_owned(), request.output.kind()))
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no canned search for {}", request.kql))
        }
    }

    /// A synthetic corpus whose oracle files live in a temporary directory.
    struct TestCorpus {
        directory: tempfile::TempDir,
        corpus: Corpus,
    }

    impl TestCorpus {
        /// Factory function.
        ///
        /// # Returns
        ///
        /// An empty corpus.
        fn new() -> Self {
            let directory = tempfile::tempdir().expect("a temporary directory");
            let data_root = directory.path().to_path_buf();
            Self {
                directory,
                corpus: Corpus {
                    root: prototype_root(),
                    data_root,
                    archive_oracles: Vec::new(),
                    fixtures: Vec::new(),
                    queries: Vec::new(),
                    query_oracles: Vec::new(),
                    datasets: Vec::new(),
                },
            }
        }

        /// # Returns
        ///
        /// The path of a new file named `name` holding `data`.
        fn write(&self, name: &str, data: &[u8]) -> PathBuf {
            let path = self.directory.path().join(name);
            std::fs::write(&path, data).expect("the oracle file is written");
            path
        }

        /// Adds a fixture archive: readable with the `x` oracle `oracle`, or rejected without one.
        ///
        /// # Returns
        ///
        /// The archive path the reader is asked to extract.
        fn add_fixture(&mut self, name: &str, kind: FixtureKind, oracle: Option<&[u8]>) -> PathBuf {
            let archive = self.directory.path().join(name);
            self.corpus.archive_oracles.push(ArchiveOracle {
                fixture: name.to_owned(),
                kind,
                expectation: if oracle.is_some() {
                    Expectation::Readable
                } else {
                    Expectation::Reject
                },
                archive: archive.clone(),
                flags: Vec::new(),
                oracle: oracle.map(|data| self.write(&format!("{name}.jsonl"), data)),
                canonical_sha256: oracle.map(|data| canonical_digest(data).sha256),
                num_records: 0,
            });
            archive
        }

        /// Adds a query case checked on `output`, with the C++ oracle `oracle`.
        fn add_query(&mut self, case: QueryCase, output: OutputKind, oracle: &[u8], cpp_exit: i32) {
            let file = self.write(&format!("{}.{output}", case.id), oracle);
            let digest = canonical_digest(oracle);
            let count = match output {
                OutputKind::Stdout => digest.count,
                OutputKind::Msgpack => WireTuple::decode_stream(oracle)
                    .expect("a valid msgpack oracle")
                    .len() as u64,
                OutputKind::ResultsCache => ResultsCacheDocument::parse_dump(
                    std::str::from_utf8(oracle)
                        .expect("the results-cache oracle should be UTF-8 text"),
                )
                .expect("a valid dump")
                .len() as u64,
            };
            self.corpus.query_oracles.push(QueryOracle {
                id: case.id.clone(),
                output,
                archives: vec![self.directory.path().join("archive.clps")],
                file,
                cpp_exit,
                count,
                canonical_sha256: Some(digest.sha256),
                cpp_ids: None,
                reference_ids: None,
                reference_status: None,
            });
            self.corpus.queries.push(QueryCase {
                outputs: vec![output],
                ..case
            });
        }

        /// # Returns
        ///
        /// The fixture reports of `reader`.
        fn run_fixtures(&self, reader: &FakeReader) -> Vec<CaseReport> {
            let registry = Registry::load(&prototype_root()).expect("the registry loads");
            let cpp = CppClpS::default();
            Harness::new(
                &self.corpus,
                &registry,
                &cpp,
                reader,
                self.directory.path(),
                HarnessOptions::default(),
            )
            .run_fixtures()
        }

        /// # Returns
        ///
        /// The query reports of `reader`.
        fn run_queries(&self, reader: &FakeReader) -> Vec<CaseReport> {
            self.run_queries_with(reader, HarnessOptions::default())
        }

        /// # Returns
        ///
        /// The query reports of `reader` under `options`.
        fn run_queries_with(
            &self,
            reader: &FakeReader,
            options: HarnessOptions,
        ) -> Vec<CaseReport> {
            let registry = Registry::load(&prototype_root()).expect("the registry loads");
            let cpp = CppClpS::default();
            Harness::new(
                &self.corpus,
                &registry,
                &cpp,
                reader,
                self.directory.path(),
                options,
            )
            .run_queries()
        }
    }

    /// # Returns
    ///
    /// A fixture query case without divergences.
    fn case(id: &str, kql: &str) -> QueryCase {
        QueryCase {
            id: id.to_owned(),
            target: QueryTarget::Fixture("search.fake".to_owned()),
            kql: kql.to_owned(),
            ..BASE_CASE
        }
    }

    /// # Returns
    ///
    /// A JSON-lines output.
    fn jsonl(data: &[u8]) -> SearchOutput {
        SearchOutput::JsonLines(data.to_vec())
    }

    /// # Returns
    ///
    /// Text record keys.
    fn ids(texts: &[&str]) -> Vec<RecordKey> {
        texts
            .iter()
            .map(|text| RecordKey::Text((*text).to_owned()))
            .collect()
    }

    /// # Returns
    ///
    /// A wire tuple whose message is `message` plus a newline.
    fn tuple(timestamp_ms: i64, message: &str, archive_id: &str, log_event_idx: i64) -> WireTuple {
        WireTuple {
            timestamp_ms,
            message: format!("{message}\n").into_bytes(),
            orig_file_path: Vec::new(),
            archive_id: archive_id.as_bytes().to_vec(),
            log_event_idx,
        }
    }

    /// # Returns
    ///
    /// The tuples encoded like the C++ `file --path` output.
    fn encode_tuples(tuples: &[WireTuple]) -> Vec<u8> {
        tuples
            .iter()
            .flat_map(|tuple| {
                rmp_serde::to_vec(&(
                    tuple.timestamp_ms,
                    String::from_utf8_lossy(&tuple.message),
                    "",
                    String::from_utf8_lossy(&tuple.archive_id),
                    tuple.log_event_idx,
                ))
                .expect("a tuple encodes")
            })
            .collect()
    }

    /// # Returns
    ///
    /// The results-cache document of a tuple.
    fn document(tuple: &WireTuple) -> ResultsCacheDocument {
        ResultsCacheDocument {
            archive_id: String::from_utf8_lossy(&tuple.archive_id).into_owned(),
            log_event_idx: tuple.log_event_idx,
            dataset: String::new(),
            message: String::from_utf8_lossy(&tuple.message).into_owned(),
            orig_file_path: String::new(),
            timestamp: tuple.timestamp_ms,
        }
    }

    /// # Returns
    ///
    /// The documents dumped like `gen-oracles.sh` (canonical extended JSON).
    fn dump(documents: &[ResultsCacheDocument]) -> Vec<u8> {
        let values: Vec<serde_json::Value> = documents
            .iter()
            .map(|document| {
                serde_json::json!({
                    "_id": {
                        "archive_id": document.archive_id,
                        "log_event_idx": {"$numberLong": document.log_event_idx.to_string()},
                    },
                    "dataset": document.dataset,
                    "message": document.message,
                    "orig_file_path": document.orig_file_path,
                    "timestamp": {"$numberLong": document.timestamp.to_string()},
                })
            })
            .collect();
        serde_json::to_vec(&values).expect("the dump serializes")
    }

    /// # Returns
    ///
    /// The failure of `report`.
    ///
    /// # Panics
    ///
    /// Panics if the report is not a failure.
    fn failure_of(report: &CaseReport) -> &Failure {
        match &report.verdict {
            Verdict::Fail(failure) => failure,
            verdict => panic!("{} must fail, got {verdict:?}", report.case),
        }
    }

    /// # Returns
    ///
    /// The divergence ids `report` accepted.
    ///
    /// # Panics
    ///
    /// Panics if the report is not an acceptance.
    fn accepted_ids(report: &CaseReport) -> Vec<&str> {
        match &report.verdict {
            Verdict::Accepted { divergences, .. } => {
                divergences.iter().map(DivergenceId::as_str).collect()
            }
            verdict => panic!("{} must be accepted, got {verdict:?}", report.case),
        }
    }

    #[test]
    fn fixtures_pass_accept_float_normalization_or_fail() {
        let mut test = TestCorpus::new();
        let exact = test.add_fixture("exact.clps", FixtureKind::Sfa, Some(b"{\"a\":1,\"b\":2}\n"));
        let float = test.add_fixture("float.clps", FixtureKind::Sfa, Some(b"{\"f\":0.000000}\n"));
        let wrong = test.add_fixture("wrong.clps", FixtureKind::Sfa, Some(b"{\"a\":1}\n"));
        let failing = test.add_fixture("failing.clps", FixtureKind::Sfa, Some(b"{\"a\":1}\n"));
        let mut reader = FakeReader::default();
        let ok = |data: &[u8]| ExtractOutcome {
            jsonl: data.to_vec(),
            failure: None,
        };
        reader.extracts.insert(exact, ok(b"{\"b\":2,\"a\":1}\n"));
        reader.extracts.insert(float, ok(b"{\"f\":1e-7}\n"));
        reader.extracts.insert(wrong, ok(b"{\"a\":2}\n"));
        reader.extracts.insert(
            failing,
            ExtractOutcome {
                jsonl: Vec::new(),
                failure: Some(ReaderFailure {
                    kind: Some(ErrorKind::CorruptArchive),
                    exit_code: None,
                    message: "bad".to_owned(),
                }),
            },
        );
        let reports = test.run_fixtures(&reader);
        assert_eq!(reports[0].verdict, Verdict::Pass);
        assert_eq!(accepted_ids(&reports[1]), ["DV-01"]);
        let Failure::RecordMismatch { report } = failure_of(&reports[2]) else {
            panic!("{} must be a RecordMismatch", reports[2]);
        };
        assert!(report.contains("{\"a\":2}"), "{report}");
        assert!(matches!(
            failure_of(&reports[3]),
            Failure::ReaderFailed { .. }
        ));
        assert_eq!(
            Summary::of(&reports),
            Summary {
                num_passed: 1,
                num_accepted: 1,
                num_skipped: 0,
                num_failed: 2,
            }
        );
    }

    #[test]
    fn stale_oracles_are_infrastructure_failures() {
        let mut test = TestCorpus::new();
        let archive = test.add_fixture("stale.clps", FixtureKind::Sfa, Some(b"[1]\n"));
        test.corpus.archive_oracles[0].canonical_sha256 = Some("0".repeat(64));
        let mut reader = FakeReader::default();
        reader.extracts.insert(
            archive,
            ExtractOutcome {
                jsonl: b"[1]\n".to_vec(),
                failure: None,
            },
        );
        let reports = test.run_fixtures(&reader);
        let Failure::Infrastructure { error } = failure_of(&reports[0]) else {
            panic!("{} must be an infrastructure failure", reports[0]);
        };
        assert!(error.contains("differs from its manifest"), "{error}");
    }

    #[test]
    fn rejection_fixtures_must_fail_with_unsupported_archive_before_output() {
        let mut test = TestCorpus::new();
        let mfa = test.add_fixture("a.mfa", FixtureKind::RejectMfa, None);
        let structured = test.add_fixture("b.clps", FixtureKind::RejectSfa, None);
        let read = test.add_fixture("c.clps", FixtureKind::RejectSfa, None);
        let corrupt = test.add_fixture("d.clps", FixtureKind::RejectSfa, None);
        let late = test.add_fixture("e.clps", FixtureKind::RejectSfa, None);
        let failure = |kind| ReaderFailure {
            kind: Some(kind),
            exit_code: None,
            message: "rejected".to_owned(),
        };
        let mut reader = FakeReader::default();
        reader.extracts.insert(
            mfa,
            ExtractOutcome {
                jsonl: Vec::new(),
                failure: Some(failure(ErrorKind::UnsupportedArchive)),
            },
        );
        reader.extracts.insert(
            structured,
            ExtractOutcome {
                jsonl: Vec::new(),
                failure: Some(ReaderFailure {
                    kind: None,
                    exit_code: Some(2),
                    message: String::new(),
                }),
            },
        );
        reader.extracts.insert(
            read,
            ExtractOutcome {
                jsonl: b"[1]\n[2]\n".to_vec(),
                failure: None,
            },
        );
        reader.extracts.insert(
            corrupt,
            ExtractOutcome {
                jsonl: Vec::new(),
                failure: Some(failure(ErrorKind::CorruptArchive)),
            },
        );
        reader.extracts.insert(
            late,
            ExtractOutcome {
                jsonl: b"[1]\n".to_vec(),
                failure: Some(failure(ErrorKind::UnsupportedArchive)),
            },
        );
        let reports = test.run_fixtures(&reader);
        assert_eq!(accepted_ids(&reports[0]), ["SX-01"]);
        assert_eq!(accepted_ids(&reports[1]), ["SX-02"]);
        assert_eq!(
            failure_of(&reports[2]),
            &Failure::NotRejected { num_records: 2 }
        );
        assert!(matches!(
            failure_of(&reports[3]),
            Failure::WrongRejection { .. }
        ));
        assert!(matches!(
            failure_of(&reports[4]),
            Failure::WrongRejection { .. }
        ));
    }

    #[test]
    fn undeclared_stdout_cases_compare_canonical_multisets() {
        let mut test = TestCorpus::new();
        test.add_query(
            case("same", "a: 1"),
            OutputKind::Stdout,
            b"{\"a\":1,\"b\":[1, 2]}\n",
            0,
        );
        test.add_query(
            case("float", "f: 1"),
            OutputKind::Stdout,
            b"{\"f\":1.500000}\n",
            0,
        );
        test.add_query(case("diff", "a: 2"), OutputKind::Stdout, b"{\"a\":2}\n", 0);
        test.add_query(
            case("failed", "a: 3"),
            OutputKind::Stdout,
            b"{\"a\":3}\n",
            0,
        );
        let mut reader = FakeReader::default()
            .with_search(
                "a: 1",
                OutputKind::Stdout,
                jsonl(b"{\"b\":[1,2],\"a\":1}\n"),
            )
            .with_search("f: 1", OutputKind::Stdout, jsonl(b"{\"f\":1.5}\n"))
            .with_search("a: 2", OutputKind::Stdout, jsonl(b"{\"a\":2}\n{\"a\":2}\n"));
        reader.searches.insert(
            ("a: 3".to_owned(), OutputKind::Stdout),
            SearchOutcome {
                output: jsonl(b""),
                failure: Some(ReaderFailure {
                    kind: Some(ErrorKind::Io),
                    exit_code: None,
                    message: "io".to_owned(),
                }),
            },
        );
        let reports = test.run_queries(&reader);
        assert_eq!(reports[0].verdict, Verdict::Pass);
        assert_eq!(accepted_ids(&reports[1]), ["DV-01"]);
        let Failure::RecordMismatch { report } = failure_of(&reports[2]) else {
            panic!("{} must be a RecordMismatch", reports[2]);
        };
        assert!(report.contains("1 unexpected"), "{report}");
        assert!(matches!(
            failure_of(&reports[3]),
            Failure::ReaderFailed { .. }
        ));
    }

    #[test]
    fn declared_cases_need_the_expected_ids_and_a_cpp_difference() {
        let declared = |id: &str, kql: &str| QueryCase {
            divergences: vec!["DV-11".to_owned()],
            expected_rust_ids: Some(ids(&["r1", "r3"])),
            ..case(id, kql)
        };
        let mut test = TestCorpus::new();
        test.add_fixture(
            "search.fake",
            FixtureKind::Sfa,
            Some(b"{\"rid\":\"r1\",\"v\":1}\n{\"rid\":\"r2\",\"v\":2}\n{\"rid\":\"r3\",\"v\":3}\n"),
        );
        for (id, kql) in [
            ("ok", "q1"),
            ("same", "q2"),
            ("ids", "q3"),
            ("content", "q4"),
            ("reader-only-content", "q5"),
        ] {
            test.add_query(declared(id, kql), OutputKind::Stdout, SEARCH_RECORDS, 0);
        }
        let expected = b"{\"rid\":\"r1\",\"v\":1}\n{\"rid\":\"r3\",\"v\":3}\n";
        let reader = FakeReader::default()
            .with_search("q1", OutputKind::Stdout, jsonl(expected))
            .with_search("q2", OutputKind::Stdout, jsonl(SEARCH_RECORDS))
            .with_search(
                "q3",
                OutputKind::Stdout,
                jsonl(b"{\"rid\":\"r1\",\"v\":1}\n"),
            )
            .with_search(
                "q4",
                OutputKind::Stdout,
                jsonl(b"{\"rid\":\"r1\",\"v\":9}\n{\"rid\":\"r3\",\"v\":3}\n"),
            )
            .with_search(
                "q5",
                OutputKind::Stdout,
                jsonl(b"{\"rid\":\"r1\",\"v\":1}\n{\"rid\":\"r3\",\"v\":9}\n"),
            );
        let reports = test.run_queries(&reader);
        assert_eq!(accepted_ids(&reports[0]), ["DV-11"]);
        assert!(matches!(
            failure_of(&reports[1]),
            Failure::DivergenceNotReproduced { .. }
        ));
        let Failure::ExpectationMismatch { detail } = failure_of(&reports[2]) else {
            panic!("{} must be a ExpectationMismatch", reports[2]);
        };
        assert!(detail.contains("missing [\"r3\"]"), "{detail}");
        assert!(matches!(
            failure_of(&reports[3]),
            Failure::RecordMismatch { .. }
        ));
        let Failure::RecordMismatch { report } = failure_of(&reports[4]) else {
            panic!("{} must be a RecordMismatch", reports[4]);
        };
        assert!(report.contains("\"v\":9"), "{report}");
    }

    #[test]
    fn declared_ids_must_agree_with_the_recorded_reference_model() {
        let mut test = TestCorpus::new();
        let case = QueryCase {
            divergences: vec!["DV-11".to_owned()],
            expected_rust_ids: Some(ids(&["r1"])),
            ..case("reference", "q")
        };
        test.add_query(case, OutputKind::Stdout, SEARCH_RECORDS, 0);
        test.corpus.query_oracles[0].reference_ids = Some(ids(&["r2"]));
        let reader = FakeReader::default().with_search(
            "q",
            OutputKind::Stdout,
            jsonl(b"{\"rid\":\"r1\",\"v\":1}\n"),
        );
        let reports = test.run_queries(&reader);
        assert!(matches!(
            failure_of(&reports[0]),
            Failure::ReferenceDisagreement { .. }
        ));
    }

    #[test]
    fn declared_ids_with_a_stale_oracle_are_infrastructure_failures() {
        let mut test = TestCorpus::new();
        let case = QueryCase {
            divergences: vec!["DV-11".to_owned()],
            expected_rust_ids: Some(ids(&["r1"])),
            ..case("stale", "q")
        };
        test.add_query(case, OutputKind::Stdout, SEARCH_RECORDS, 0);
        test.corpus.query_oracles[0].canonical_sha256 = Some("0".repeat(64));
        let reader = FakeReader::default().with_search(
            "q",
            OutputKind::Stdout,
            jsonl(b"{\"rid\":\"r1\",\"v\":1}\n"),
        );
        let reports = test.run_queries(&reader);
        let Failure::Infrastructure { error } = failure_of(&reports[0]) else {
            panic!("{} must be an infrastructure failure", reports[0]);
        };
        assert!(error.contains("differs from its manifest"), "{error}");
    }

    #[test]
    fn declared_counts_and_exit_codes_are_checked() {
        let mut test = TestCorpus::new();
        let counted = |id: &str, kql: &str| QueryCase {
            divergences: vec!["DV-05".to_owned()],
            expected_rust_count: Some(1),
            ..case(id, kql)
        };
        let exiting = |id: &str, kql: &str| QueryCase {
            divergences: vec!["DV-20".to_owned()],
            expected_rust_exit: Some(0),
            ..case(id, kql)
        };
        test.add_query(
            counted("count-ok", "c1"),
            OutputKind::Stdout,
            SEARCH_RECORDS,
            0,
        );
        test.add_query(
            counted("count-bad", "c2"),
            OutputKind::Stdout,
            SEARCH_RECORDS,
            0,
        );
        test.add_query(exiting("exit-ok", "e1"), OutputKind::Stdout, b"", 1);
        test.add_query(exiting("exit-bad", "e2"), OutputKind::Stdout, b"", 1);
        test.add_query(exiting("exit-same", "e3"), OutputKind::Stdout, b"", 0);
        let mut reader = FakeReader::default()
            .with_search(
                "c1",
                OutputKind::Stdout,
                jsonl(b"{\"rid\":\"r1\",\"v\":1}\n"),
            )
            .with_search("c2", OutputKind::Stdout, jsonl(b"[1]\n[2]\n[3]\n"))
            .with_search("e1", OutputKind::Stdout, jsonl(b""))
            .with_search("e3", OutputKind::Stdout, jsonl(b""));
        reader.searches.insert(
            ("e2".to_owned(), OutputKind::Stdout),
            SearchOutcome {
                output: jsonl(b""),
                failure: Some(ReaderFailure {
                    kind: None,
                    exit_code: Some(1),
                    message: String::new(),
                }),
            },
        );
        let reports = test.run_queries(&reader);
        assert_eq!(accepted_ids(&reports[0]), ["DV-05"]);
        assert!(matches!(
            failure_of(&reports[1]),
            Failure::ExpectationMismatch { .. }
        ));
        assert_eq!(accepted_ids(&reports[2]), ["DV-20"]);
        assert!(matches!(
            failure_of(&reports[3]),
            Failure::ExpectationMismatch { .. }
        ));
        assert!(matches!(
            failure_of(&reports[4]),
            Failure::DivergenceNotReproduced { .. }
        ));
    }

    #[test]
    fn declared_counts_are_checked_against_the_reference_evaluator() -> anyhow::Result<()> {
        const MATCHES: &[u8] = b"{\"a\":{}}\n{\"a\":{\"b\":{}}}\n\
            {\"a\":{\"b\":{\"c\":{\"d\":{\"e\":{\"f\":{\"g\":{\"h\":1}}}}}}}}\n\
            {\"a\":{\"b\":1,\"c\":{\"d\":\"x\"}},\"e\":{}}\n";
        const WRONG_CONTENT: &[u8] = b"{\"a\":{}}\n{\"a\":{\"b\":{}}}\n{\"a\":{\"b\":2}}\n\
            {\"a\":{\"b\":1,\"c\":{\"d\":\"x\"}},\"e\":{}}\n";

        let mut test = TestCorpus::new();
        let fixture = Corpus::load(&prototype_root())?
            .fixture("nested.default")
            .cloned()
            .expect("the corpus has nested.default");
        test.corpus.fixtures.push(fixture);
        let counted = |id: &str, kql: &str, expected_count: u64| QueryCase {
            target: QueryTarget::Fixture("nested.default".to_owned()),
            divergences: vec!["DV-19".to_owned()],
            expected_rust_count: Some(expected_count),
            ..case(id, kql)
        };
        let cpp_oracle = b"{\"a\":{\"b\":1,\"c\":{\"d\":\"x\"}},\"e\":{}}\n";
        test.add_query(counted("ok", "a: *", 4), OutputKind::Stdout, cpp_oracle, 0);
        test.add_query(
            counted("content", "a:*", 4),
            OutputKind::Stdout,
            cpp_oracle,
            0,
        );
        test.add_query(
            counted("count", "(a: *)", 5),
            OutputKind::Stdout,
            cpp_oracle,
            0,
        );
        let mut five = MATCHES.to_vec();
        five.extend_from_slice(b"{\"a\":{}}\n");
        let reader = FakeReader::default()
            .with_search("a: *", OutputKind::Stdout, jsonl(MATCHES))
            .with_search("a:*", OutputKind::Stdout, jsonl(WRONG_CONTENT))
            .with_search("(a: *)", OutputKind::Stdout, jsonl(&five));

        let enabled = HarnessOptions {
            is_reference_evaluator_enabled: true,
            ..HarnessOptions::default()
        };
        let reports = test.run_queries_with(&reader, enabled);
        assert_eq!(accepted_ids(&reports[0]), ["DV-19"]);
        for report in &reports[1..] {
            assert!(
                matches!(failure_of(report), Failure::ReferenceDisagreement { .. }),
                "{report}"
            );
        }

        let reports = test.run_queries(&reader);
        assert_eq!(accepted_ids(&reports[1]), ["DV-19"]);
        Ok(())
    }

    #[test]
    fn invalid_cases_are_reported() {
        let mut test = TestCorpus::new();
        let unregistered = QueryCase {
            divergences: vec!["DV-99".to_owned()],
            ..case("unregistered", "u")
        };
        let no_expectation = QueryCase {
            divergences: vec!["DV-11".to_owned()],
            ..case("no-expectation", "n")
        };
        test.add_query(unregistered, OutputKind::Stdout, SEARCH_RECORDS, 0);
        test.add_query(no_expectation, OutputKind::Stdout, SEARCH_RECORDS, 0);
        test.corpus.queries.push(QueryCase {
            outputs: vec![OutputKind::Stdout],
            ..case("no-oracle", "o")
        });
        let reader = FakeReader::default()
            .with_search("u", OutputKind::Stdout, jsonl(b"[1]\n"))
            .with_search("n", OutputKind::Stdout, jsonl(b"[1]\n"));
        let reports = test.run_queries(&reader);
        assert_eq!(reports.len(), 3);
        for report in &reports {
            assert!(
                matches!(failure_of(report), Failure::InvalidCase { .. }),
                "{report}"
            );
        }
    }

    #[test]
    fn msgpack_cases_compare_tuples_or_check_the_session_file() {
        let first = tuple(5, "{\"rid\":\"r1\"}", "a.clps", 0);
        let second = tuple(6, "{\"rid\":\"r2\"}", "b.clps", 0);
        let mut test = TestCorpus::new();
        test.add_query(
            case("same", "m1"),
            OutputKind::Msgpack,
            &encode_tuples(&[first.clone(), second.clone()]),
            0,
        );
        test.add_query(
            case("diff", "m2"),
            OutputKind::Msgpack,
            &encode_tuples(std::slice::from_ref(&first)),
            0,
        );
        let session = |id: &str, kql: &str| QueryCase {
            divergences: vec!["DV-21".to_owned()],
            ..case(id, kql)
        };
        test.add_query(
            session("session-ok", "m3"),
            OutputKind::Msgpack,
            &encode_tuples(std::slice::from_ref(&second)),
            0,
        );
        test.add_query(
            session("session-bad", "m4"),
            OutputKind::Msgpack,
            &encode_tuples(std::slice::from_ref(&second)),
            0,
        );
        let mut moved = first.clone();
        moved.log_event_idx = 9;
        let both = SearchOutput::Msgpack(vec![first.clone(), second.clone()]);
        let reader = FakeReader::default()
            .with_search(
                "m1",
                OutputKind::Msgpack,
                SearchOutput::Msgpack(vec![second.clone(), first]),
            )
            .with_search(
                "m2",
                OutputKind::Msgpack,
                SearchOutput::Msgpack(vec![moved]),
            )
            .with_search("m3", OutputKind::Msgpack, both)
            .with_search(
                "m3",
                OutputKind::Stdout,
                jsonl(b"{\"rid\":\"r1\"}\n{\"rid\":\"r2\"}\n"),
            )
            .with_search(
                "m4",
                OutputKind::Msgpack,
                SearchOutput::Msgpack(vec![second]),
            )
            .with_search(
                "m4",
                OutputKind::Stdout,
                jsonl(b"{\"rid\":\"r1\"}\n{\"rid\":\"r2\"}\n"),
            );
        let reports = test.run_queries(&reader);
        assert_eq!(reports[0].verdict, Verdict::Pass);
        let Failure::RecordMismatch { report } = failure_of(&reports[1]) else {
            panic!("{} must be a RecordMismatch", reports[1]);
        };
        assert!(report.contains("log_event_idx=9"), "{report}");
        assert_eq!(accepted_ids(&reports[2]), ["DV-21"]);
        assert!(matches!(
            failure_of(&reports[3]),
            Failure::ExpectationMismatch { .. }
        ));
    }

    #[test]
    fn results_cache_cases_compare_documents_or_check_the_session_top_k() {
        let matches: Vec<WireTuple> = (0..5)
            .map(|index| tuple(100 + index, &format!("{{\"i\":{index}}}"), "a.clps", index))
            .collect();
        let top: Vec<ResultsCacheDocument> = matches[3..].iter().rev().map(document).collect();
        let per_archive: Vec<ResultsCacheDocument> = matches[1..].iter().map(document).collect();
        let mut test = TestCorpus::new();
        test.add_query(case("same", "r1"), OutputKind::ResultsCache, &dump(&top), 0);
        let session = |id: &str, kql: &str| QueryCase {
            divergences: vec!["DV-25".to_owned()],
            max_num_results: 2,
            ..case(id, kql)
        };
        test.add_query(
            session("topk-ok", "r2"),
            OutputKind::ResultsCache,
            &dump(&per_archive),
            0,
        );
        test.add_query(
            session("topk-bad", "r3"),
            OutputKind::ResultsCache,
            &dump(&per_archive),
            0,
        );
        test.add_query(
            case("unsupported", "r4"),
            OutputKind::ResultsCache,
            &dump(&top),
            0,
        );
        let wrong: Vec<ResultsCacheDocument> = matches[..2].iter().map(document).collect();
        let reader = FakeReader::default()
            .with_search(
                "r1",
                OutputKind::ResultsCache,
                SearchOutput::ResultsCache(top.clone()),
            )
            .with_search(
                "r2",
                OutputKind::ResultsCache,
                SearchOutput::ResultsCache(top),
            )
            .with_search(
                "r2",
                OutputKind::Msgpack,
                SearchOutput::Msgpack(matches.clone()),
            )
            .with_search(
                "r3",
                OutputKind::ResultsCache,
                SearchOutput::ResultsCache(wrong),
            )
            .with_search("r3", OutputKind::Msgpack, SearchOutput::Msgpack(matches))
            .with_search("r4", OutputKind::ResultsCache, SearchOutput::Unsupported);
        let reports = test.run_queries(&reader);
        assert_eq!(reports[0].verdict, Verdict::Pass);
        assert_eq!(accepted_ids(&reports[1]), ["DV-25"]);
        assert!(matches!(
            failure_of(&reports[2]),
            Failure::ExpectationMismatch { .. }
        ));
        assert!(matches!(reports[3].verdict, Verdict::Skipped { .. }));
    }

    #[test]
    fn the_case_filter_selects_cases() {
        let mut test = TestCorpus::new();
        test.add_query(case("one", "a: 1"), OutputKind::Stdout, b"[1]\n", 0);
        test.add_query(case("two", "a: 2"), OutputKind::Stdout, b"[2]\n", 0);
        let reader = FakeReader::default().with_search("a: 2", OutputKind::Stdout, jsonl(b"[2]\n"));
        let registry = Registry::load(&prototype_root()).expect("the registry loads");
        let cpp = CppClpS::default();
        let options = HarnessOptions {
            case_filter: vec!["two".to_owned()],
            ..HarnessOptions::default()
        };
        let reports = Harness::new(
            &test.corpus,
            &registry,
            &cpp,
            &reader,
            test.directory.path(),
            options,
        )
        .run_queries();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].to_string(), "PASS     two [stdout]");
    }

    #[test]
    fn top_k_violations_are_found() {
        let matches: Vec<WireTuple> = [(10, 0), (20, 1), (20, 2), (30, 3)]
            .iter()
            .map(|(timestamp, index)| tuple(*timestamp, &format!("[{index}]"), "a", *index))
            .collect();
        let documents = |indices: &[usize]| -> Vec<ResultsCacheDocument> {
            indices
                .iter()
                .map(|index| document(&matches[*index]))
                .collect()
        };
        assert_eq!(find_top_k_violation(&documents(&[3, 1]), &matches, 2), None);
        assert_eq!(find_top_k_violation(&documents(&[3, 2]), &matches, 2), None);
        assert_eq!(
            find_top_k_violation(&documents(&[0, 1, 2, 3]), &matches, 10),
            None
        );
        assert_eq!(find_top_k_violation(&[], &[], 10), None);
        let violation = |documents: &[ResultsCacheDocument]| {
            find_top_k_violation(documents, &matches, 2).unwrap_or_default()
        };
        assert!(violation(&documents(&[3])).contains("expected 2 documents"));
        assert!(violation(&documents(&[3, 0])).contains("older than"));
        assert!(violation(&documents(&[1, 2])).contains("newer than"));
        let mut collision = documents(&[3, 1]);
        collision[1].log_event_idx = 3;
        assert!(violation(&collision).contains("collide"));
        let mut foreign = documents(&[3, 1]);
        foreign[1].message = "[9]\n".to_owned();
        assert!(violation(&foreign).contains("not a match"));
    }
}
