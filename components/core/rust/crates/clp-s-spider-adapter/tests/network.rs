//! End-to-end tests of the Spider search task's protocol-v2 network output, ignored by default
//! since they need a C++ `clp-s` with the `network --session-token --task-index` output (`$CLP_S`,
//! else the repository's `build/core/clp-s`): for the suite's fixture queries, the stream the
//! in-process search sends is byte-for-byte the stream of the C++ `clp-s` subprocess, up to the
//! registered divergences, and every in-process attempt sends the same bytes; unsupported archives
//! fall back to `clp-s`, which streams with the task's network arguments; and a task whose stream
//! the listener closes without acknowledging fails, on every path.
//!
//! The CLP home's `bin/clp-s` is a wrapper of the C++ binary that logs every invocation, so the
//! tests also check which path searched each archive.

use std::collections::HashMap;
use std::io::Read;
use std::io::Write;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::num::NonZeroU16;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::thread::JoinHandle;

use clp_rust_utils::clp_config::package::config::ArchiveOutput;
use clp_rust_utils::clp_config::package::config::ArchiveOutputStorage;
use clp_rust_utils::clp_config::package::config::Package;
use clp_rust_utils::clp_config::package::config::SpiderTaskExecutorConfig;
use clp_rust_utils::clp_config::package::config::StorageEngine;
use clp_rust_utils::job_config::SessionToken;
use clp_rust_utils::task_io::query::ClpSQueryOption;
use clp_rust_utils::task_io::query::OutputHandle;
use clp_rust_utils::types::ArchiveId;
use clp_s_difftest::compare::RecordComparison;
use clp_s_difftest::corpus::Corpus;
use clp_s_difftest::corpus::FixtureKind;
use clp_s_difftest::corpus::QueryCase;
use clp_s_difftest::corpus::QueryTarget;
use clp_s_difftest::corpus::prototype_root;
use clp_s_difftest::cpp::default_cpp_clp_s;
use clp_s_difftest::multiset::RecordMultiset;
use clp_s_reader::ReaderContext;
use clp_s_spider_adapter::AdapterConfig;
use clp_s_spider_adapter::AdapterResources;
use clp_s_spider_adapter::ArchiveToSearch;
use clp_s_spider_adapter::search;
use clp_s_spider_adapter::search_with_clp_s_subprocess;
use non_empty_string::NonEmptyString;
use spider_core::types::id::JobId;
use spider_core::types::id::ResourceGroupId;
use spider_core::types::id::TaskId;
use spider_tdl::TaskContext;

const DATASET: &str = "e2e";
const SESSION_TOKEN: &str = "6f1c1c64-1e0a-4be8-a7a6-6a0e4d3c2b1a";
const TASK_INDEX: u64 = 17;

/// One decoded handshake: `(version, session_token, task_index, archive_id)`.
type Handshake = (u8, String, u64, String);

/// One decoded result frame: `(result_index, timestamp_ms, message)`.
type Frame = (u64, i64, String);

/// One decoded connection: the handshake, the result frames and the trailer's count, if any.
type Stream = (Handshake, Vec<Frame>, Option<u64>);

/// How the streams of the two paths compared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Agreement {
    num_cases: usize,
    num_frames: usize,

    /// Cases whose two streams are the same bytes.
    identical_cases: usize,

    /// Frames whose messages are equal only after the plain-float normalization of DV-01/DV-02.
    float_normalized_messages: usize,

    /// Frames whose timestamps differ only by DV-27's flooring of pre-1970 milliseconds.
    floored_timestamps: usize,
}

/// The process-wide test environment: a CLP home with a logging `clp-s` wrapper, the staged
/// archives and the adapter resources.
struct Harness {
    clp_home: tempfile::TempDir,

    /// Keeps the runtime of the adapter resources alive.
    _runtime: tokio::runtime::Runtime,

    corpus: Corpus,

    /// The archive id each staged archive was given, by archive path.
    staged: Mutex<HashMap<PathBuf, ArchiveId>>,

    /// Held while a task runs, so that the `clp-s` wrapper's log tells which task spawned it.
    streaming: Mutex<()>,
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
    /// Sets up the CLP home and installs the adapter resources.
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
            .worker_threads(2)
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

        Self {
            clp_home,
            _runtime: runtime,
            corpus: Corpus::load(&prototype_root()).expect("the corpus loads"),
            staged: Mutex::new(HashMap::new()),
            streaming: Mutex::new(()),
        }
    }

    /// Links `archive` (a file or a multi-file archive directory) into the dataset's archive
    /// directory under a fresh archive id.
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
        staged.insert(archive.to_path_buf(), archive_id);
        archive_id
    }

    /// # Returns
    ///
    /// The number of times the `clp-s` wrapper ran so far.
    fn num_clp_s_runs(&self) -> usize {
        std::fs::read_to_string(self.clp_home.path().join("clp-s.log"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    /// Runs one search task over `archive_id` that streams to a fresh capturing listener, which
    /// acknowledges every stream, in-process (with its fallback) or with the `clp-s` subprocess.
    ///
    /// # Returns
    ///
    /// The bytes of every connection the task made, and whether the `clp-s` wrapper ran, on
    /// success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::run`]'s return values on failure.
    /// * Forwards the task's error on failure.
    fn stream(
        &self,
        archive_id: ArchiveId,
        option: &ClpSQueryOption,
        is_subprocess: bool,
    ) -> anyhow::Result<(Vec<Vec<u8>>, bool)> {
        let (result, connections, has_run_clp_s) =
            self.run(archive_id, option, is_subprocess, true)?;
        result?;
        Ok((connections, has_run_clp_s))
    }

    /// Runs one search task over `archive_id` that streams to a fresh capturing listener, which
    /// closes every stream without acknowledging it.
    ///
    /// # Returns
    ///
    /// The task's error and the bytes of every connection the task made, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if the task succeeds.
    /// * Forwards [`Self::run`]'s return values on failure.
    fn stream_without_acknowledgement(
        &self,
        archive_id: ArchiveId,
        option: &ClpSQueryOption,
        is_subprocess: bool,
    ) -> anyhow::Result<(anyhow::Error, Vec<Vec<u8>>)> {
        let (result, connections, _) = self.run(archive_id, option, is_subprocess, false)?;
        let Err(error) = result else {
            anyhow::bail!("the task should fail without an acknowledgement");
        };
        Ok((error, connections))
    }

    /// Runs one search task over `archive_id` that streams to a fresh capturing listener,
    /// in-process (with its fallback) or with the `clp-s` subprocess.
    ///
    /// # Returns
    ///
    /// A tuple on success, containing:
    ///
    /// * The task's result.
    /// * The bytes of every connection the task made.
    /// * Whether the `clp-s` wrapper ran.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if the capture thread panics.
    /// * Forwards [`TaskContext::new`]'s return values on failure.
    /// * Forwards [`Capture::new`]'s return values on failure.
    /// * Forwards [`Capture::finish`]'s return values on failure.
    fn run(
        &self,
        archive_id: ArchiveId,
        option: &ClpSQueryOption,
        is_subprocess: bool,
        is_acknowledging: bool,
    ) -> anyhow::Result<(anyhow::Result<()>, Vec<Vec<u8>>, bool)> {
        let ctx = TaskContext::new(
            JobId::random(),
            TaskId::Index(0),
            1,
            ResourceGroupId::random(),
            None,
        )?;
        let capture = Capture::new(is_acknowledging)?;
        let output_handle = OutputHandle::Network {
            host: NonEmptyString::new(capture.address.ip().to_string())
                .expect("the host is not empty"),
            port: NonZeroU16::new(capture.address.port()).expect("the bound port is not zero"),
            session_token: SESSION_TOKEN.parse::<SessionToken>()?,
        };
        let task = if is_subprocess {
            search_with_clp_s_subprocess
        } else {
            search
        };
        let _streaming = self
            .streaming
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let num_clp_s_runs = self.num_clp_s_runs();
        let result = task(
            &ctx,
            &config(),
            1,
            option,
            ArchiveToSearch {
                id: archive_id,
                dataset: Some(DATASET),
                task_index: TASK_INDEX,
            },
            &output_handle,
        );
        let connections = capture.finish()?;
        Ok((result, connections, self.num_clp_s_runs() > num_clp_s_runs))
    }
}

/// A listener capturing every byte of the connections made to it.
struct Capture {
    address: SocketAddr,
    connections: JoinHandle<anyhow::Result<Vec<Vec<u8>>>>,
}

impl Capture {
    /// Factory function.
    ///
    /// Binds a listener on a free local port that reads each connection to its end, and
    /// acknowledges the results its trailer counts if `is_acknowledging`, until a connection
    /// closes without sending anything.
    ///
    /// # Returns
    ///
    /// The newly created capture on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`TcpListener::bind`]'s return values on failure.
    /// * Forwards [`TcpListener::local_addr`]'s return values on failure.
    fn new(is_acknowledging: bool) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let connections = std::thread::spawn(move || {
            let mut connections = Vec::new();
            loop {
                let (mut stream, _) = listener.accept()?;
                let mut received = Vec::new();
                stream.read_to_end(&mut received)?;
                if received.is_empty() {
                    return Ok(connections);
                }
                if is_acknowledging {
                    let num_results = decode(&received)?
                        .2
                        .ok_or_else(|| anyhow::anyhow!("the stream has no trailer"))?;
                    let mut ack = vec![0x91, 0xcf];
                    ack.extend_from_slice(&num_results.to_be_bytes());
                    stream.write_all(&ack)?;
                }
                connections.push(received);
            }
        });
        Ok(Self {
            address,
            connections,
        })
    }

    /// Ends the capture once the task is done.
    ///
    /// # Returns
    ///
    /// The bytes received on each connection, in connection order, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if:
    ///   * The capture thread panicked.
    ///   * The capture failed to accept, read, decode or acknowledge a connection.
    /// * Forwards [`std::net::TcpStream::connect`]'s return values on failure.
    fn finish(self) -> anyhow::Result<Vec<Vec<u8>>> {
        drop(std::net::TcpStream::connect(self.address)?);
        self.connections
            .join()
            .map_err(|_| anyhow::anyhow!("the capture thread panicked"))?
    }
}

/// # Returns
///
/// The executor config of a package storing its archives on the file system under `archives`.
fn config() -> SpiderTaskExecutorConfig {
    SpiderTaskExecutorConfig {
        package: Package {
            storage_engine: StorageEngine::ClpS,
            ..Package::default()
        },
        archive_output: ArchiveOutput {
            storage: ArchiveOutputStorage::Fs {
                directory: "archives".to_owned(),
            },
            ..ArchiveOutput::default()
        },
        ..SpiderTaskExecutorConfig::default()
    }
}

/// Decodes the bytes of one connection.
///
/// # Returns
///
/// The handshake, the result frames and the trailer's count, if the connection has a trailer, on
/// success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if bytes follow the trailer.
/// * Forwards [`rmp_serde::decode::from_read`]'s return values on failure (`data` is not a
///   handshake followed by frames and an optional trailer).
fn decode(data: &[u8]) -> anyhow::Result<Stream> {
    let mut remaining = data;
    let handshake = rmp_serde::decode::from_read(&mut remaining)?;
    let mut frames = Vec::new();
    while !remaining.is_empty() {
        if 0x91 == remaining[0] {
            let (num_results,): (u64,) = rmp_serde::decode::from_read(&mut remaining)?;
            anyhow::ensure!(remaining.is_empty(), "bytes follow the trailer");
            return Ok((handshake, frames, Some(num_results)));
        }
        frames.push(rmp_serde::decode::from_read(&mut remaining)?);
    }
    Ok((handshake, frames, None))
}

/// Compares the C++ stream with the Rust stream of the same task: the same handshake, then the
/// same frames in the same order, with messages equal up to DV-01/DV-02 and timestamps equal up
/// to DV-27, then the same trailer.
///
/// # Returns
///
/// How the streams agree on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * A description of the mismatch if the streams differ beyond the registered divergences.
fn compare_streams(cpp: &[Vec<u8>], rust: &[Vec<u8>]) -> Result<Agreement, String> {
    let mut agreement = Agreement {
        num_cases: 1,
        ..Agreement::default()
    };
    if cpp == rust {
        agreement.identical_cases = 1;
    }
    match (cpp, rust) {
        ([], []) => return Ok(agreement),
        ([cpp], [rust]) => {
            let (cpp_handshake, cpp_frames, cpp_trailer) =
                decode(cpp).map_err(|error| format!("C++: {error}"))?;
            let (rust_handshake, rust_frames, rust_trailer) =
                decode(rust).map_err(|error| format!("Rust: {error}"))?;
            if cpp_handshake != rust_handshake {
                return Err(format!(
                    "handshakes differ: {cpp_handshake:?} != {rust_handshake:?}"
                ));
            }
            if cpp_trailer != rust_trailer {
                return Err(format!(
                    "trailers differ: {cpp_trailer:?} != {rust_trailer:?}"
                ));
            }
            if cpp_frames.len() != rust_frames.len() {
                return Err(format!(
                    "{} C++ and {} Rust frames",
                    cpp_frames.len(),
                    rust_frames.len()
                ));
            }
            agreement.num_frames = cpp_frames.len();
            for (cpp_frame, rust_frame) in cpp_frames.iter().zip(&rust_frames) {
                if cpp_frame.0 != rust_frame.0 {
                    return Err(format!("indices differ: {cpp_frame:?} != {rust_frame:?}"));
                }
                if cpp_frame.1 != rust_frame.1 {
                    if !(cpp_frame.1 <= 0 && rust_frame.1 == cpp_frame.1 - 1) {
                        return Err(format!(
                            "timestamps differ: {cpp_frame:?} != {rust_frame:?}"
                        ));
                    }
                    agreement.floored_timestamps += 1;
                }
                if cpp_frame.2 != rust_frame.2 {
                    let comparison = RecordComparison::compare(
                        &RecordMultiset::from_jsonl(cpp_frame.2.as_bytes()),
                        &RecordMultiset::from_jsonl(rust_frame.2.as_bytes()),
                    );
                    if !comparison.is_explained() {
                        return Err(format!(
                            "messages of frame {} differ: {}",
                            cpp_frame.0,
                            comparison.render(4)
                        ));
                    }
                    agreement.float_normalized_messages += 1;
                }
            }
        }
        _ => {
            return Err(format!(
                "{} C++ and {} Rust connections",
                cpp.len(),
                rust.len()
            ));
        }
    }
    Ok(agreement)
}

/// # Returns
///
/// The fixture query cases whose streams both paths must agree on: single-archive fixture targets
/// the reader supports, with no declared divergence, no projection (the task has none) and no
/// expected failure, with the archive each runs on.
fn fixture_cases(corpus: &Corpus) -> Vec<(&QueryCase, PathBuf)> {
    corpus
        .queries
        .iter()
        .filter(|case| {
            !case.is_divergent()
                && case.options.projection.is_empty()
                && case.expected_rust_exit.is_none()
        })
        .filter_map(|case| {
            let QueryTarget::Fixture(name) = &case.target else {
                return None;
            };
            let fixture = corpus.fixture(name)?;
            if !matches!(fixture.kind, FixtureKind::Sfa | FixtureKind::Kvir) {
                return None;
            }
            match fixture.archives.as_slice() {
                [archive] => Some((case, archive.clone())),
                _ => None,
            }
        })
        .collect()
}

/// # Returns
///
/// The task's query option running `case`, with a result limit the network output ignores.
fn query_option(case: &QueryCase) -> ClpSQueryOption {
    ClpSQueryOption {
        query_string: NonEmptyString::new(case.kql.clone()).expect("the query is not empty"),
        max_num_results: std::num::NonZeroU32::new(3),
        begin_timestamp_millisecs: case.options.time_begin_ms,
        end_timestamp_millisecs: case.options.time_end_ms,
        ignore_case: case.options.is_case_insensitive,
    }
}

#[test]
#[ignore = "needs a C++ clp-s with the protocol-v2 network output (`CLP_S`)"]
fn fs_network_streams_equal_the_clp_s_subprocess() -> anyhow::Result<()> {
    let harness = Harness::get();
    let cases = fixture_cases(&harness.corpus);
    anyhow::ensure!(
        cases.len() > 100,
        "too few comparable cases: {}",
        cases.len()
    );
    let mut agreement = Agreement::default();
    let mut failures = Vec::new();
    for (case, archive) in &cases {
        let archive_id = harness.stage(archive);
        let option = query_option(case);
        let (rust, has_rust_run_clp_s) = harness.stream(archive_id, &option, false)?;
        let (rust_again, _) = harness.stream(archive_id, &option, false)?;
        let (cpp, _) = harness.stream(archive_id, &option, true)?;
        let outcome = if has_rust_run_clp_s {
            Err("the in-process path spawned clp-s".to_owned())
        } else if rust != rust_again {
            Err("two in-process attempts sent different bytes".to_owned())
        } else {
            compare_streams(&cpp, &rust)
        };
        match outcome {
            Ok(case_agreement) => {
                agreement.num_cases += case_agreement.num_cases;
                agreement.num_frames += case_agreement.num_frames;
                agreement.identical_cases += case_agreement.identical_cases;
                agreement.float_normalized_messages += case_agreement.float_normalized_messages;
                agreement.floored_timestamps += case_agreement.floored_timestamps;
            }
            Err(mismatch) => failures.push(format!("{} ({}): {mismatch}", case.id, case.kql)),
        }
    }
    println!(
        "{} cases, {} failures; {agreement:?}",
        cases.len(),
        failures.len()
    );
    anyhow::ensure!(failures.is_empty(), "{}", failures.join("\n"));
    anyhow::ensure!(
        agreement.num_frames > 1000,
        "too few frames were compared: {agreement:?}"
    );
    Ok(())
}

#[test]
#[ignore = "needs a C++ clp-s with the protocol-v2 network output (`CLP_S`)"]
fn network_unsupported_archives_fall_back_to_clp_s() -> anyhow::Result<()> {
    let harness = Harness::get();
    let reject = prototype_root().join("tests/fixtures/archives/reject");
    for (archive, num_records) in [
        (reject.join("scalars.mfa"), 11),
        (reject.join("arrays.structurize-arrays.clps"), 20),
    ] {
        let archive_id = harness.stage(&archive);
        let option = ClpSQueryOption {
            query_string: NonEmptyString::new("*".to_owned()).expect("the query is not empty"),
            max_num_results: None,
            begin_timestamp_millisecs: None,
            end_timestamp_millisecs: None,
            ignore_case: false,
        };
        let (connections, has_run_clp_s) = harness.stream(archive_id, &option, false)?;
        assert!(has_run_clp_s, "{}: the task fell back", archive.display());
        let [connection] = connections.as_slice() else {
            panic!("{}: {} connections", archive.display(), connections.len());
        };
        let (handshake, frames, num_results) = decode(connection)?;
        assert_eq!(
            handshake,
            (
                2,
                SESSION_TOKEN.to_owned(),
                TASK_INDEX,
                archive_id.to_string()
            )
        );
        assert_eq!(num_results, Some(num_records), "{}", archive.display());
        assert_eq!(
            frames.iter().map(|frame| frame.0).collect::<Vec<_>>(),
            (0..num_records).collect::<Vec<_>>(),
            "{}",
            archive.display()
        );
        let (cpp_connections, _) = harness.stream(archive_id, &option, true)?;
        assert_eq!(cpp_connections, connections);
    }
    Ok(())
}

#[test]
#[ignore = "needs a C++ clp-s with the protocol-v2 network output (`CLP_S`)"]
fn network_tasks_fail_without_an_acknowledgement() -> anyhow::Result<()> {
    let harness = Harness::get();
    let archives = prototype_root().join("tests/fixtures/archives");
    let option = ClpSQueryOption {
        query_string: NonEmptyString::new("*".to_owned()).expect("the query is not empty"),
        max_num_results: None,
        begin_timestamp_millisecs: None,
        end_timestamp_millisecs: None,
        ignore_case: false,
    };
    for (archive, is_subprocess) in [
        (archives.join("search.ts.clps"), false),
        (archives.join("search.ts.clps"), true),
        (archives.join("reject/scalars.mfa"), false),
    ] {
        let archive_id = harness.stage(&archive);
        let (error, connections) =
            harness.stream_without_acknowledgement(archive_id, &option, is_subprocess)?;
        let [connection] = connections.as_slice() else {
            panic!("{}: {} connections", archive.display(), connections.len());
        };
        let (_, frames, num_results) = decode(connection)?;
        assert!(!frames.is_empty(), "{}", archive.display());
        assert_eq!(
            num_results,
            Some(frames.len() as u64),
            "{}: {error:?}",
            archive.display()
        );
    }
    Ok(())
}
