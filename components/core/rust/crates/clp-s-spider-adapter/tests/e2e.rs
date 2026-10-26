//! End-to-end tests of the Spider search task, ignored by default
//! (`crates/clp-s-spider-adapter/scripts/run-e2e-tests.sh` starts the `MongoDB` and S3 servers they
//! need): over file-system and S3 archive storage, the documents the in-process search writes into
//! the results cache equal the documents of the C++ `clp-s` subprocess path for the suite's
//! fixture queries (and, with a result limit, for the dataset queries); retried tasks insert no
//! duplicates; unsupported archives fall back to `clp-s`.
//!
//! The CLP home's `bin/clp-s` is a wrapper of the C++ binary that logs every invocation, so the
//! tests also check which path searched each archive.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;

use aws_sdk_s3::operation::create_bucket::CreateBucketError;
use clp_rust_utils::clp_config::AwsAuthentication;
use clp_rust_utils::clp_config::AwsCredentials;
use clp_rust_utils::clp_config::S3Config;
use clp_rust_utils::clp_config::package::config::ArchiveOutput;
use clp_rust_utils::clp_config::package::config::ArchiveOutputStorage;
use clp_rust_utils::clp_config::package::config::Package;
use clp_rust_utils::clp_config::package::config::SpiderTaskExecutorConfig;
use clp_rust_utils::clp_config::package::config::StorageEngine;
use clp_rust_utils::task_io::query::ArchiveToSearch;
use clp_rust_utils::task_io::query::ClpSQueryOption;
use clp_rust_utils::task_io::query::OutputHandle;
use clp_rust_utils::types::ArchiveId;
use clp_rust_utils::types::non_empty_string::ExpectedNonEmpty;
use clp_s_difftest::compare::RecordComparison;
use clp_s_difftest::corpus::Corpus;
use clp_s_difftest::corpus::QueryCase;
use clp_s_difftest::corpus::QueryTarget;
use clp_s_difftest::corpus::prototype_root;
use clp_s_difftest::cpp::default_cpp_clp_s;
use clp_s_difftest::multiset::RecordMultiset;
use clp_s_reader::ReaderContext;
use clp_s_spider_adapter::AdapterConfig;
use clp_s_spider_adapter::AdapterResources;
use clp_s_spider_adapter::search;
use clp_s_spider_adapter::search_with_clp_s_subprocess;
use mongodb::bson::Document;
use non_empty_string::NonEmptyString;
use spider_core::types::id::JobId;
use spider_core::types::id::ResourceGroupId;
use spider_core::types::id::TaskId;
use spider_tdl::TaskContext;

/// The environment variable naming the test `MongoDB` server's URI (with a database).
const MONGODB_URI_VARIABLE: &str = "CLP_S_TEST_MONGODB_URI";

/// The environment variable naming the test S3 server's endpoint; its credentials are
/// `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`.
const S3_ENDPOINT_VARIABLE: &str = "CLP_S_TEST_S3_ENDPOINT";

/// The `--max-num-results` default of the C++ binary under test: it predates commit 2883feea,
/// which made the default unlimited (the behaviour `ClpSQueryOption::max_num_results` documents),
/// so a task without a limit keeps only the latest 1,000 results on the subprocess path.
const CPP_BINARY_DEFAULT_LIMIT: usize = 1000;

const DATASET: &str = "e2e";
const BUCKET: &str = "proto-spider";
const KEY_PREFIX: &str = "LIB1/";

/// Where the test archives live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Storage {
    Fs,
    S3,
}

/// One results-cache document.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct CacheDocument {
    archive_id: String,
    log_event_idx: i64,
    dataset: String,
    orig_file_path: String,
    timestamp: i64,
    message: String,
}

impl CacheDocument {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The fields of the results-cache document `document`.
    ///
    /// # Panics
    ///
    /// Panics if `document` lacks a field of a results-cache document.
    fn from_bson(document: &Document) -> Self {
        let id = document
            .get_document("_id")
            .expect("the document has an _id");
        let text = |document: &Document, key: &str| {
            document
                .get_str(key)
                .unwrap_or_else(|_| panic!("the document has the string field `{key}`"))
                .to_owned()
        };
        let integer = |document: &Document, key: &str| {
            document
                .get_i64(key)
                .unwrap_or_else(|_| panic!("the document has the int64 field `{key}`"))
        };
        Self {
            archive_id: text(id, "archive_id"),
            log_event_idx: integer(id, "log_event_idx"),
            dataset: text(document, "dataset"),
            orig_file_path: text(document, "orig_file_path"),
            timestamp: integer(document, "timestamp"),
            message: text(document, "message"),
        }
    }

    /// # Returns
    ///
    /// The document's `_id`: its archive id and log event index.
    fn key(&self) -> (&str, i64) {
        (&self.archive_id, self.log_event_idx)
    }
}

/// How two sets of documents compared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Agreement {
    num_documents: usize,

    /// Messages equal only after the plain-float normalization of DV-01/DV-02.
    float_normalized_messages: usize,

    /// Timestamps that differ only by DV-27's flooring of pre-1970 milliseconds.
    floored_timestamps: usize,

    /// Documents that differ only in which of several records tied at the k-th timestamp were kept
    /// (DV-25).
    tied_documents: usize,

    /// Cases without a limit whose C++ documents are the latest [`CPP_BINARY_DEFAULT_LIMIT`]
    /// results, compared with the latest Rust documents.
    capped_cases: usize,
}

impl Agreement {
    /// Adds every counter of `other` to `self`.
    const fn add(&mut self, other: Self) {
        self.num_documents += other.num_documents;
        self.float_normalized_messages += other.float_normalized_messages;
        self.floored_timestamps += other.floored_timestamps;
        self.tied_documents += other.tied_documents;
        self.capped_cases += other.capped_cases;
    }
}

/// The process-wide test environment: a CLP home with a logging `clp-s` wrapper, the staged
/// archives, the adapter resources and the clients of the test servers.
struct Harness {
    clp_home: tempfile::TempDir,
    runtime: tokio::runtime::Runtime,
    corpus: Corpus,
    mongodb_uri: NonEmptyString,
    database: mongodb::Database,
    s3_endpoint: String,
    credentials: AwsCredentials,
    s3_client: aws_sdk_s3::Client,

    /// The archive id each staged archive was given, by archive path.
    staged: Mutex<HashMap<PathBuf, ArchiveId>>,

    next_job_id: AtomicI32,
}

impl Harness {
    /// # Returns
    ///
    /// The harness, set up on first use.
    ///
    /// # Panics
    ///
    /// Panics if the environment cannot be set up.
    fn get() -> &'static Self {
        static HARNESS: OnceLock<Harness> = OnceLock::new();
        HARNESS.get_or_init(Self::new)
    }

    /// Factory function.
    ///
    /// Sets up the CLP home, installs the adapter resources and creates the test bucket.
    ///
    /// # Returns
    ///
    /// The harness.
    ///
    /// # Panics
    ///
    /// Panics if the environment cannot be set up.
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;

        let clp_home = tempfile::tempdir().expect("a temporary CLP home can be created");
        let bin = clp_home.path().join("bin");
        std::fs::create_dir_all(&bin).expect("the bin directory can be created");
        let wrapper = bin.join("clp-s");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
                clp_home.path().join("clp-s.log").display(),
                default_cpp_clp_s().display()
            ),
        )
        .expect("the clp-s wrapper can be written");
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
            .expect("the clp-s wrapper can be made executable");
        std::fs::create_dir_all(clp_home.path().join("archives").join(DATASET))
            .expect("the dataset directory can be created");

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("a multi-thread runtime can be built");
        AdapterResources::init(
            runtime.handle().clone(),
            ReaderContext::builder()
                .memory_budget_size(Some(1 << 30))
                .build()
                .expect("the reader context is valid"),
            AdapterConfig {
                clp_home: clp_home.path().to_path_buf(),
                base_admission_timeout_sec: 60,
            },
        )
        .expect("the adapter resources can be installed");
        let resources = AdapterResources::global().expect("the resources are installed");

        let mongodb_uri = std::env::var(MONGODB_URI_VARIABLE)
            .unwrap_or_else(|_| panic!("{MONGODB_URI_VARIABLE} names the test MongoDB server"));
        let database = resources
            .results_cache(&mongodb_uri)
            .expect("the MongoDB URI is valid");
        let s3_endpoint = std::env::var(S3_ENDPOINT_VARIABLE)
            .unwrap_or_else(|_| panic!("{S3_ENDPOINT_VARIABLE} names the test S3 server"));
        let credentials = AwsCredentials {
            access_key_id: std::env::var("AWS_ACCESS_KEY_ID")
                .expect("AWS_ACCESS_KEY_ID names the test server's access key"),
            secret_access_key: std::env::var("AWS_SECRET_ACCESS_KEY")
                .expect("AWS_SECRET_ACCESS_KEY names the test server's secret key"),
            session_token: None,
        };
        let s3_client = resources.s3_client(
            "us-east-1",
            Some(&non_empty(&s3_endpoint)),
            &AwsAuthentication::Credentials {
                credentials: credentials.clone(),
            },
        );
        runtime.block_on(async {
            let result = s3_client.create_bucket().bucket(BUCKET).send().await;
            if let Err(error) = result {
                let is_owned = error
                    .as_service_error()
                    .is_some_and(CreateBucketError::is_bucket_already_owned_by_you);
                assert!(is_owned, "the test bucket cannot be created: {error:?}");
            }
        });

        Self {
            clp_home,
            runtime,
            corpus: Corpus::load(&prototype_root()).expect("the corpus loads"),
            mongodb_uri: non_empty(&mongodb_uri),
            database,
            s3_endpoint,
            credentials,
            s3_client,
            staged: Mutex::new(HashMap::new()),
            next_job_id: AtomicI32::new(
                i32::try_from(std::process::id() % 1000).unwrap_or(0) * 1_000_000,
            ),
        }
    }

    /// Stages `archive` (a file or a multi-file archive directory) under a fresh archive id: linked
    /// into the dataset's archive directory, and, if it is a file, uploaded to the S3 bucket.
    ///
    /// # Returns
    ///
    /// The archive id.
    ///
    /// # Panics
    ///
    /// Panics if the archive cannot be staged.
    fn stage(&self, archive: &Path) -> ArchiveId {
        let mut staged = self.staged.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(archive_id) = staged.get(archive) {
            return *archive_id;
        }
        let archive_id: ArchiveId = format!("00000000-0000-4000-8000-{:012x}", staged.len())
            .parse()
            .expect("the archive id is a UUID");
        std::os::unix::fs::symlink(
            archive,
            self.clp_home
                .path()
                .join("archives")
                .join(DATASET)
                .join(archive_id.to_string()),
        )
        .expect("the archive can be linked into the dataset directory");
        if archive.is_file() {
            let bytes = std::fs::read(archive).expect("the archive is readable");
            self.runtime
                .block_on(
                    self.s3_client
                        .put_object()
                        .bucket(BUCKET)
                        .key(format!("{KEY_PREFIX}{DATASET}/{archive_id}"))
                        .body(bytes.into())
                        .send(),
                )
                .expect("the archive can be uploaded");
        }
        staged.insert(archive.to_path_buf(), archive_id);
        archive_id
    }

    /// # Returns
    ///
    /// The executor config of a package storing its archives on `storage`.
    fn config(&self, storage: Storage) -> SpiderTaskExecutorConfig {
        let storage = match storage {
            Storage::Fs => ArchiveOutputStorage::Fs {
                directory: "archives".to_owned(),
            },
            Storage::S3 => ArchiveOutputStorage::S3 {
                staging_directory: "staging".to_owned(),
                s3_config: S3Config {
                    bucket: non_empty(BUCKET),
                    region_code: None,
                    key_prefix: non_empty(KEY_PREFIX),
                    endpoint_url: Some(non_empty(&self.s3_endpoint)),
                    aws_authentication: AwsAuthentication::Credentials {
                        credentials: self.credentials.clone(),
                    },
                },
            },
        };
        SpiderTaskExecutorConfig {
            package: Package {
                storage_engine: StorageEngine::ClpS,
                ..Package::default()
            },
            archive_output: ArchiveOutput {
                storage,
                ..ArchiveOutput::default()
            },
            ..SpiderTaskExecutorConfig::default()
        }
    }

    /// # Returns
    ///
    /// A query job id no other task of the process uses.
    fn next_job_id(&self) -> i32 {
        self.next_job_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Runs one search task into the collection of `query_job_id`, in-process (with its fallback)
    /// or with the `clp-s` subprocess.
    ///
    /// # Errors
    ///
    /// Returns an error if the task fails.
    fn run_task(
        &self,
        storage: Storage,
        query_job_id: i32,
        archive_id: ArchiveId,
        option: &ClpSQueryOption,
        is_subprocess: bool,
    ) -> anyhow::Result<()> {
        let ctx = TaskContext::new(
            JobId::random(),
            TaskId::Index(0),
            1,
            ResourceGroupId::random(),
            None,
        )?;
        let output_handle = OutputHandle::ResultsCache {
            uri: self.mongodb_uri.clone(),
        };
        let task = if is_subprocess {
            search_with_clp_s_subprocess
        } else {
            search
        };
        task(
            &ctx,
            &self.config(storage),
            query_job_id,
            option,
            &ArchiveToSearch {
                id: archive_id,
                dataset: Some(NonEmptyString::from_static_str(DATASET)),
                task_index: 0,
            },
            &output_handle,
        )
    }

    /// # Returns
    ///
    /// The sorted documents of the collection of `query_job_id`, which is dropped afterwards.
    ///
    /// # Panics
    ///
    /// Panics if the collection cannot be read.
    fn take_documents(&self, query_job_id: i32) -> Vec<CacheDocument> {
        let collection = self
            .database
            .collection::<Document>(&query_job_id.to_string());
        let mut documents = self.runtime.block_on(async {
            let mut cursor = collection
                .find(Document::new())
                .await
                .expect("the collection can be read");
            let mut documents = Vec::new();
            while cursor.advance().await.expect("the cursor advances") {
                documents.push(CacheDocument::from_bson(
                    &cursor
                        .deserialize_current()
                        .expect("the document deserializes"),
                ));
            }
            collection
                .drop()
                .await
                .expect("the collection can be dropped");
            documents
        });
        documents.sort();
        documents
    }

    /// # Returns
    ///
    /// Whether the `clp-s` wrapper ran for the collection of `query_job_id`.
    fn has_clp_s_run_for(&self, query_job_id: i32) -> bool {
        let log =
            std::fs::read_to_string(self.clp_home.path().join("clp-s.log")).unwrap_or_default();
        let collection = query_job_id.to_string();
        log.lines().any(|line| {
            let mut words = line.split_whitespace();
            words.any(|word| "--collection" == word) && words.next() == Some(collection.as_str())
        })
    }

    /// Runs `option` over `archive_id` in-process and with the `clp-s` subprocess, and compares
    /// the documents.
    ///
    /// # Returns
    ///
    /// How the documents agree on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * A description of the mismatch if the in-process path spawns `clp-s`, or if either path
    ///   fails.
    /// * Forwards [`compare_documents`]'s return values on failure.
    fn compare_paths(
        &self,
        storage: Storage,
        archive_id: ArchiveId,
        option: &ClpSQueryOption,
    ) -> Result<Agreement, String> {
        let rust_job_id = self.next_job_id();
        let cpp_job_id = self.next_job_id();
        let rust = self.run_task(storage, rust_job_id, archive_id, option, false);
        let cpp = self.run_task(storage, cpp_job_id, archive_id, option, true);
        let rust_documents = self.take_documents(rust_job_id);
        let cpp_documents = self.take_documents(cpp_job_id);
        if self.has_clp_s_run_for(rust_job_id) {
            return Err("the in-process path spawned clp-s".to_owned());
        }
        match (rust, cpp) {
            (Ok(()), Ok(())) => {}
            (Err(rust), Err(cpp)) => {
                return Err(format!("both paths failed: Rust `{rust:#}`, C++ `{cpp:#}`"));
            }
            (rust, cpp) => {
                return Err(format!(
                    "only one path failed: Rust {:?}, C++ {:?}",
                    rust.err().map(|error| format!("{error:#}")),
                    cpp.err().map(|error| format!("{error:#}"))
                ));
            }
        }
        if option.max_num_results.is_none()
            && CPP_BINARY_DEFAULT_LIMIT == cpp_documents.len()
            && rust_documents.len() > CPP_BINARY_DEFAULT_LIMIT
        {
            let mut latest = rust_documents;
            latest.sort_by_key(|document| std::cmp::Reverse(document.timestamp));
            latest.truncate(CPP_BINARY_DEFAULT_LIMIT);
            latest.sort();
            let mut agreement = compare_documents(
                &cpp_documents,
                &latest,
                NonZeroU32::new(u32::try_from(CPP_BINARY_DEFAULT_LIMIT).unwrap_or(u32::MAX)),
            )?;
            agreement.capped_cases += 1;
            return Ok(agreement);
        }
        compare_documents(&cpp_documents, &rust_documents, option.max_num_results)
    }
}

/// # Returns
///
/// `text` as a non-empty string.
///
/// # Panics
///
/// Panics if `text` is empty.
fn non_empty(text: &str) -> NonEmptyString {
    NonEmptyString::new(text.to_owned()).expect("the text is not empty")
}

/// Compares the C++ documents with the Rust documents of the same task: the same `_id`s, datasets
/// and file paths; messages equal up to DV-01/DV-02; timestamps equal up to DV-27. Under a result
/// limit, documents may differ only among records tied at the k-th timestamp (DV-25).
///
/// # Returns
///
/// How the documents agree on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * A description of the mismatch if the documents differ beyond the registered divergences.
fn compare_documents(
    cpp: &[CacheDocument],
    rust: &[CacheDocument],
    limit: Option<NonZeroU32>,
) -> Result<Agreement, String> {
    let mut agreement = Agreement {
        num_documents: cpp.len(),
        ..Agreement::default()
    };
    let cpp_by_key: BTreeMap<_, _> = cpp
        .iter()
        .map(|document| (document.key(), document))
        .collect();
    let rust_by_key: BTreeMap<_, _> = rust
        .iter()
        .map(|document| (document.key(), document))
        .collect();
    if cpp_by_key.len() != cpp.len() || rust_by_key.len() != rust.len() {
        return Err("duplicate `_id`s".to_owned());
    }
    let only_cpp: Vec<_> = cpp_by_key
        .iter()
        .filter(|(key, _)| !rust_by_key.contains_key(*key))
        .map(|(_, document)| *document)
        .collect();
    let only_rust: Vec<_> = rust_by_key
        .iter()
        .filter(|(key, _)| !cpp_by_key.contains_key(*key))
        .map(|(_, document)| *document)
        .collect();
    if !only_cpp.is_empty() || !only_rust.is_empty() {
        let boundary = cpp.iter().map(|document| document.timestamp).min();
        let is_tie = limit.is_some()
            && cpp.len() == rust.len()
            && only_cpp
                .iter()
                .chain(&only_rust)
                .all(|document| Some(document.timestamp) == boundary);
        if !is_tie {
            return Err(format!(
                "{} C++ and {} Rust documents; only in C++: {only_cpp:?}; only in Rust: \
                 {only_rust:?}",
                cpp.len(),
                rust.len()
            ));
        }
        agreement.tied_documents += only_cpp.len();
    }
    for (key, cpp_document) in &cpp_by_key {
        let Some(rust_document) = rust_by_key.get(key) else {
            continue;
        };
        if cpp_document.dataset != rust_document.dataset
            || cpp_document.orig_file_path != rust_document.orig_file_path
        {
            return Err(format!("{cpp_document:?} != {rust_document:?}"));
        }
        if cpp_document.timestamp != rust_document.timestamp {
            let is_floored = cpp_document.timestamp <= 0
                && rust_document.timestamp == cpp_document.timestamp - 1;
            if !is_floored {
                return Err(format!(
                    "timestamps differ: {cpp_document:?} != {rust_document:?}"
                ));
            }
            agreement.floored_timestamps += 1;
        }
        if cpp_document.message != rust_document.message {
            let comparison = RecordComparison::compare(
                &RecordMultiset::from_jsonl(cpp_document.message.as_bytes()),
                &RecordMultiset::from_jsonl(rust_document.message.as_bytes()),
            );
            if !comparison.is_explained() {
                return Err(format!("messages differ: {}", comparison.render(4)));
            }
            agreement.float_normalized_messages += 1;
        }
    }
    Ok(agreement)
}

/// # Returns
///
/// The fixture query cases whose results both paths must agree on: single-archive fixture targets
/// with log order, no declared divergence, no projection (the task has none) and no expected
/// failure, with the archive each runs on.
fn fixture_cases(corpus: &Corpus) -> Vec<(&QueryCase, PathBuf)> {
    corpus
        .queries
        .iter()
        .filter(|case| is_comparable(case))
        .filter_map(|case| {
            let QueryTarget::Fixture(name) = &case.target else {
                return None;
            };
            let fixture = corpus.fixture(name)?;
            let has_log_order = !fixture
                .flags
                .iter()
                .any(|flag| flag == "--disable-log-order");
            match fixture.archives.as_slice() {
                [archive] if has_log_order => Some((case, archive.clone())),
                _ => None,
            }
        })
        .collect()
}

/// # Returns
///
/// The dataset query cases whose results both paths must agree on, with the dataset archive each
/// runs on.
fn dataset_cases(corpus: &Corpus) -> Vec<(&QueryCase, PathBuf)> {
    corpus
        .queries
        .iter()
        .filter(|case| is_comparable(case))
        .filter_map(|case| {
            let QueryTarget::Dataset(name) = &case.target else {
                return None;
            };
            Some((case, corpus.dataset(name)?.archive.clone()))
        })
        .collect()
}

/// # Returns
///
/// Whether both paths must agree on `case`: it declares no divergence, has no projection (the
/// task has none) and expects no failure.
const fn is_comparable(case: &QueryCase) -> bool {
    !case.is_divergent() && case.options.projection.is_empty() && case.expected_rust_exit.is_none()
}

/// # Returns
///
/// The task's query option running `case` with result limit `limit`.
fn query_option(case: &QueryCase, limit: Option<NonZeroU32>) -> ClpSQueryOption {
    ClpSQueryOption {
        query_string: non_empty(&case.kql),
        max_num_results: limit,
        begin_timestamp_millisecs: case.options.time_begin_ms,
        end_timestamp_millisecs: case.options.time_end_ms,
        ignore_case: case.options.is_case_insensitive,
    }
}

/// Runs `cases` on `storage` with result limit `limit`, and fails with every mismatch.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] listing every case whose documents differ between the two paths.
fn ensure_paths_agree(
    storage: Storage,
    limit: Option<NonZeroU32>,
    cases: &[(&QueryCase, PathBuf)],
) -> anyhow::Result<()> {
    let harness = Harness::get();
    let mut agreement = Agreement::default();
    let mut failures = Vec::new();
    for (case, archive) in cases {
        let archive_id = harness.stage(archive);
        match harness.compare_paths(storage, archive_id, &query_option(case, limit)) {
            Ok(case_agreement) => agreement.add(case_agreement),
            Err(mismatch) => failures.push(format!("{} ({}): {mismatch}", case.id, case.kql)),
        }
    }
    println!(
        "{storage:?}, limit {limit:?}: {} cases, {} failures; {agreement:?}",
        cases.len(),
        failures.len()
    );
    anyhow::ensure!(failures.is_empty(), "{}", failures.join("\n"));
    Ok(())
}

/// Runs every comparable fixture case on `storage` with result limit `limit`.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if the corpus has too few comparable cases.
/// * Forwards [`ensure_paths_agree`]'s return values on failure.
fn ensure_fixture_paths_agree(storage: Storage, limit: Option<NonZeroU32>) -> anyhow::Result<()> {
    let cases = fixture_cases(&Harness::get().corpus);
    anyhow::ensure!(
        cases.len() > 100,
        "too few comparable cases: {}",
        cases.len()
    );
    ensure_paths_agree(storage, limit, &cases)
}

#[test]
#[ignore = "needs MongoDB and S3 servers (scripts/run-e2e-tests.sh)"]
fn fs_documents_equal_the_clp_s_subprocess() -> anyhow::Result<()> {
    ensure_fixture_paths_agree(Storage::Fs, None)
}

#[test]
#[ignore = "needs MongoDB and S3 servers (scripts/run-e2e-tests.sh)"]
fn s3_documents_equal_the_clp_s_subprocess() -> anyhow::Result<()> {
    ensure_fixture_paths_agree(Storage::S3, None)
}

#[test]
#[ignore = "needs MongoDB and S3 servers (scripts/run-e2e-tests.sh)"]
fn fs_top_k_documents_equal_the_clp_s_subprocess() -> anyhow::Result<()> {
    ensure_fixture_paths_agree(Storage::Fs, NonZeroU32::new(3))
}

#[test]
#[ignore = "needs MongoDB and S3 servers (scripts/run-e2e-tests.sh)"]
fn s3_top_k_documents_equal_the_clp_s_subprocess() -> anyhow::Result<()> {
    ensure_fixture_paths_agree(Storage::S3, NonZeroU32::new(10))
}

#[test]
#[ignore = "needs MongoDB and S3 servers (scripts/run-e2e-tests.sh)"]
fn dataset_top_k_documents_equal_the_clp_s_subprocess() -> anyhow::Result<()> {
    let cases = dataset_cases(&Harness::get().corpus);
    anyhow::ensure!(cases.len() > 20, "too few dataset cases: {}", cases.len());
    for storage in [Storage::Fs, Storage::S3] {
        ensure_paths_agree(storage, NonZeroU32::new(10), &cases)?;
    }
    Ok(())
}

#[test]
#[ignore = "needs MongoDB and S3 servers (scripts/run-e2e-tests.sh)"]
fn retried_tasks_insert_no_duplicates() -> anyhow::Result<()> {
    let harness = Harness::get();
    let archive = prototype_root().join("tests/fixtures/archives/logs.ts.clps");
    let archive_id = harness.stage(&archive);
    let option = ClpSQueryOption {
        query_string: non_empty("level: ERROR"),
        max_num_results: None,
        begin_timestamp_millisecs: None,
        end_timestamp_millisecs: None,
        ignore_case: false,
    };
    for storage in [Storage::Fs, Storage::S3] {
        let query_job_id = harness.next_job_id();
        harness.run_task(storage, query_job_id, archive_id, &option, false)?;
        harness.run_task(storage, query_job_id, archive_id, &option, false)?;
        let documents = harness.take_documents(query_job_id);

        let cpp_job_id = harness.next_job_id();
        harness.run_task(storage, cpp_job_id, archive_id, &option, true)?;
        let cpp_documents = harness.take_documents(cpp_job_id);
        assert!(!cpp_documents.is_empty());
        assert_eq!(
            compare_documents(&cpp_documents, &documents, None)
                .map(|agreement| agreement.num_documents),
            Ok(cpp_documents.len()),
            "{storage:?}"
        );
    }
    Ok(())
}

#[test]
#[ignore = "needs MongoDB and S3 servers (scripts/run-e2e-tests.sh)"]
fn unsupported_archives_fall_back_to_clp_s() -> anyhow::Result<()> {
    let harness = Harness::get();
    let reject = prototype_root().join("tests/fixtures/archives/reject");
    for (storage, archive, num_records) in [
        (Storage::Fs, reject.join("scalars.mfa"), 11),
        (
            Storage::Fs,
            reject.join("arrays.structurize-arrays.clps"),
            20,
        ),
        (
            Storage::S3,
            reject.join("arrays.structurize-arrays.clps"),
            20,
        ),
    ] {
        let archive_id = harness.stage(&archive);
        let option = ClpSQueryOption {
            query_string: non_empty("*"),
            max_num_results: None,
            begin_timestamp_millisecs: None,
            end_timestamp_millisecs: None,
            ignore_case: false,
        };
        let query_job_id = harness.next_job_id();
        harness.run_task(storage, query_job_id, archive_id, &option, false)?;
        assert!(
            harness.has_clp_s_run_for(query_job_id),
            "{storage:?} {}: the task fell back to clp-s",
            archive.display()
        );
        let documents = harness.take_documents(query_job_id);
        assert_eq!(
            documents.len(),
            num_records,
            "{storage:?} {}",
            archive.display()
        );
        assert!(
            documents
                .iter()
                .all(|document| document.archive_id == archive_id.to_string()
                    && DATASET == document.dataset),
            "{documents:?}"
        );

        let cpp_job_id = harness.next_job_id();
        harness.run_task(storage, cpp_job_id, archive_id, &option, true)?;
        assert_eq!(harness.take_documents(cpp_job_id), documents);
    }
    Ok(())
}
