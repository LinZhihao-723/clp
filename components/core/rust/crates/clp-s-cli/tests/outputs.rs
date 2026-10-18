//! `clp-s-rs search` outputs as a process: the msgpack file and network outputs and the results
//! cache judged against the C++ oracles by the differential harness, `--max-num-results`, and the
//! exit codes of misconfigured or unreachable outputs. The results-cache cases need a `MongoDB`
//! server and are ignored by default (`crates/clp-s-results-cache/scripts/run-mongo-tests.sh`).

use std::io::Read;
use std::net::TcpListener;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

use clp_s_difftest::corpus::Corpus;
use clp_s_difftest::corpus::prototype_root;
use clp_s_difftest::cpp::CppClpS;
use clp_s_difftest::harness::Harness;
use clp_s_difftest::harness::HarnessOptions;
use clp_s_difftest::harness::Verdict;
use clp_s_difftest::reader::ExtractOutcome;
use clp_s_difftest::reader::OutputRequest;
use clp_s_difftest::reader::ReaderFailure;
use clp_s_difftest::reader::ReaderUnderTest;
use clp_s_difftest::reader::ResultsCacheDocument;
use clp_s_difftest::reader::SearchOutcome;
use clp_s_difftest::reader::SearchOutput;
use clp_s_difftest::reader::SearchRequest;
use clp_s_difftest::reader::WireTuple;
use clp_s_difftest::registry::Registry;
use clp_s_difftest::rust::RustCliReader;
use mongodb::bson::Document;

/// The `clp-s-rs` binary under test.
const BINARY: &str = env!("CARGO_BIN_EXE_clp-s-rs");

/// The environment variable naming the test server's URI (with a database).
const MONGODB_URI_VARIABLE: &str = "CLP_S_TEST_MONGODB_URI";

/// # Returns
///
/// The path of fixture archive `name`.
fn fixture(name: &str) -> PathBuf {
    prototype_root()
        .join("tests/fixtures/archives")
        .join(format!("{name}.clps"))
}

/// # Returns
///
/// The tuples of the C++ msgpack oracle of suite case `id`, sorted.
fn msgpack_oracle(id: &str) -> Vec<WireTuple> {
    let data = std::fs::read(
        prototype_root()
            .join("tests/queries/oracles/msgpack")
            .join(format!("{id}.msgpack")),
    )
    .expect("the msgpack oracle is readable");
    sorted(WireTuple::decode_stream(&data).expect("the oracle holds msgpack tuples"))
}

/// # Returns
///
/// `tuples`, sorted.
fn sorted(mut tuples: Vec<WireTuple>) -> Vec<WireTuple> {
    tuples.sort();
    tuples
}

/// # Returns
///
/// The output of `clp-s-rs search <archives...> <args...>`.
fn search(archives: &[&Path], args: &[&str]) -> Output {
    Command::new(BINARY)
        .arg("search")
        .args(archives)
        .args(args)
        .output()
        .expect("clp-s-rs can be spawned")
}

/// `clp-s-rs` with every output: stdout and msgpack (`--output file`) through [`RustCliReader`],
/// and the results cache through `--output results-cache` into the server named by
/// [`MONGODB_URI_VARIABLE`] (unsupported when it is unset).
#[derive(Debug)]
struct OutputsCliReader {
    stdout: RustCliReader,
    runtime: tokio::runtime::Runtime,
    database: Option<mongodb::Database>,
}

impl OutputsCliReader {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The reader.
    fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a runtime can be built");
        let database = std::env::var(MONGODB_URI_VARIABLE).ok().map(|uri| {
            runtime
                .block_on(async { mongodb::Client::with_uri_str(&uri).await })
                .expect("the test URI is valid")
                .default_database()
                .expect("the test URI names a database")
        });
        Self {
            stdout: RustCliReader::new(PathBuf::from(BINARY)),
            runtime,
            database,
        }
    }

    /// # Returns
    ///
    /// The search command of `request`, without its output arguments.
    fn command(request: &SearchRequest<'_>) -> Command {
        let options = request.options;
        let mut command = Command::new(BINARY);
        command
            .arg("s")
            .args(request.archives)
            .arg(format!("--query={}", request.kql));
        if options.is_case_insensitive {
            command.arg("--ignore-case");
        }
        if let Some(begin) = options.time_begin_ms {
            command.arg(format!("--tge={begin}"));
        }
        if let Some(end) = options.time_end_ms {
            command.arg(format!("--tle={end}"));
        }
        if !options.projection.is_empty() {
            command.arg("--projection").args(&options.projection);
        }
        command
    }

    /// # Returns
    ///
    /// Every document of `collection`, which is dropped afterwards, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`mongodb::Collection::find`]'s return values on failure.
    /// * Forwards [`mongodb::Cursor::advance`]'s return values on failure.
    /// * Forwards [`mongodb::Cursor::deserialize_current`]'s return values on failure.
    /// * Forwards [`mongodb::Collection::drop`]'s return values on failure.
    fn take_collection(
        &self,
        database: &mongodb::Database,
        collection: &str,
    ) -> anyhow::Result<Vec<ResultsCacheDocument>> {
        let collection = database.collection::<Document>(collection);
        self.runtime.block_on(async {
            let mut cursor = collection.find(Document::new()).await?;
            let mut documents = Vec::new();
            while cursor.advance().await? {
                let document = cursor.deserialize_current()?;
                let id = document.get_document("_id")?;
                documents.push(ResultsCacheDocument {
                    archive_id: id.get_str("archive_id")?.to_owned(),
                    log_event_idx: id.get_i64("log_event_idx")?,
                    dataset: document.get_str("dataset")?.to_owned(),
                    message: document.get_str("message")?.to_owned(),
                    orig_file_path: document.get_str("orig_file_path")?.to_owned(),
                    timestamp: document.get_i64("timestamp")?,
                });
            }
            collection.drop().await?;
            Ok(documents)
        })
    }
}

impl ReaderUnderTest for OutputsCliReader {
    fn name(&self) -> &'static str {
        "rust-cli-outputs"
    }

    fn extract(&self, archive: &Path, work_dir: &Path) -> anyhow::Result<ExtractOutcome> {
        self.stdout.extract(archive, work_dir)
    }

    fn search(
        &self,
        request: &SearchRequest<'_>,
        work_dir: &Path,
    ) -> anyhow::Result<SearchOutcome> {
        let OutputRequest::ResultsCache {
            max_num_results,
            dataset,
        } = &request.output
        else {
            return self.stdout.search(request, work_dir);
        };
        let (Ok(uri), Some(database)) = (std::env::var(MONGODB_URI_VARIABLE), &self.database)
        else {
            return Ok(SearchOutcome {
                output: SearchOutput::Unsupported,
                failure: None,
            });
        };
        let collection = format!("outputs_{}", std::process::id());
        let mut command = Self::command(request);
        command
            .args(["--output", "results-cache", "--uri", &uri, "--collection"])
            .arg(&collection)
            .arg(format!("--max-num-results={max_num_results}"));
        if let Some(dataset) = dataset {
            command.arg(format!("--dataset={dataset}"));
        }
        let output = command.output()?;
        let exit_code = output.status.code().unwrap_or(-1);
        let failure = (0 != exit_code).then(|| ReaderFailure {
            kind: None,
            exit_code: Some(exit_code),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
        Ok(SearchOutcome {
            output: SearchOutput::ResultsCache(self.take_collection(database, &collection)?),
            failure,
        })
    }
}

/// Runs the harness over the suite cases `ids` with [`OutputsCliReader`] and asserts that each
/// passes or is accepted under its registered divergences.
fn assert_cases_pass(ids: &[&str]) -> anyhow::Result<()> {
    let root = prototype_root();
    let corpus = Corpus::load(&root)?;
    let registry = Registry::load(&root)?;
    let cpp = CppClpS::default();
    let reader = OutputsCliReader::new();
    let work = tempfile::tempdir()?;
    let harness = Harness::new(
        &corpus,
        &registry,
        &cpp,
        &reader,
        work.path(),
        HarnessOptions {
            case_filter: ids.iter().map(|id| (*id).to_owned()).collect(),
            ..HarnessOptions::default()
        },
    );
    let reports = harness.run_queries();
    let mut judged: Vec<&str> = reports.iter().map(|report| report.case.as_str()).collect();
    judged.sort_unstable();
    judged.dedup();
    let mut expected = ids.to_vec();
    expected.sort_unstable();
    assert_eq!(judged, expected);
    for report in &reports {
        assert!(
            matches!(report.verdict, Verdict::Pass | Verdict::Accepted { .. }),
            "{report}"
        );
    }
    Ok(())
}

#[test]
fn msgpack_file_outputs_match_the_cpp_oracles() -> anyhow::Result<()> {
    assert_cases_pass(&[
        "wire-msgpack-kvir",
        "wire-msgpack-nolog",
        "wire-msgpack-search",
        "wire-msgpack-split",
    ])
}

#[test]
#[ignore = "needs a MongoDB server (crates/clp-s-results-cache/scripts/run-mongo-tests.sh)"]
fn results_cache_outputs_match_the_cpp_oracles() -> anyhow::Result<()> {
    assert!(
        std::env::var(MONGODB_URI_VARIABLE).is_ok(),
        "{MONGODB_URI_VARIABLE} names the test server"
    );
    assert_cases_pass(&["wire-cache-nolog", "wire-cache-split", "wire-cache-topk"])
}

#[test]
fn network_outputs_send_the_cpp_tuples_over_one_connection() -> anyhow::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port().to_string();
    let peer = std::thread::spawn(move || -> std::io::Result<(Vec<u8>, bool)> {
        let (mut stream, _) = listener.accept()?;
        let mut received = Vec::new();
        stream.read_to_end(&mut received)?;
        listener.set_nonblocking(true)?;
        Ok((received, listener.accept().is_ok()))
    });
    let output = search(
        &[&fixture("search.ts")],
        &[
            "-q",
            "level: WARN",
            "--output",
            "network",
            "--host",
            "127.0.0.1",
            "--port",
            &port,
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, Vec::<u8>::new());
    let (received, has_second_connection) = peer
        .join()
        .map_err(|_| anyhow::anyhow!("the peer thread panicked"))??;
    assert!(!has_second_connection);
    assert_eq!(
        sorted(WireTuple::decode_stream(&received)?),
        msgpack_oracle("wire-msgpack-search")
    );
    Ok(())
}

#[test]
fn file_outputs_replace_existing_files_once_per_session() -> anyhow::Result<()> {
    let work = tempfile::tempdir()?;
    let path = work.path().join("results.msgpack");
    std::fs::write(&path, b"stale bytes that are not msgpack tuples")?;
    let archives: Vec<PathBuf> = (0..9)
        .map(|index| fixture(&format!("logs.ts-split.{index}")))
        .collect();
    let archives: Vec<&Path> = archives.iter().map(PathBuf::as_path).collect();
    let path_arg = path.to_string_lossy().into_owned();
    let output = search(
        &archives,
        &[
            "-q",
            "level: ERROR AND service: auth",
            "--output",
            "file",
            "--path",
            &path_arg,
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tuples = WireTuple::decode_stream(&std::fs::read(&path)?)?;
    let stdout = search(&archives, &["-q", "level: ERROR AND service: auth"]).stdout;
    let mut expected: Vec<Vec<u8>> = stdout
        .split_inclusive(|byte| b'\n' == *byte)
        .map(<[u8]>::to_vec)
        .collect();
    expected.sort();
    let mut messages: Vec<Vec<u8>> = tuples.into_iter().map(|tuple| tuple.message).collect();
    messages.sort();
    assert_eq!(messages, expected);
    Ok(())
}

#[test]
fn max_num_results_keeps_the_latest_matches_of_the_session() -> anyhow::Result<()> {
    let archives: Vec<PathBuf> = (0..9)
        .map(|index| fixture(&format!("logs.ts-split.{index}")))
        .collect();
    let archives: Vec<&Path> = archives.iter().map(PathBuf::as_path).collect();
    let output = search(&archives, &["-q", "*", "--max-num-results", "10"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let documents = ResultsCacheDocument::parse_dump(&std::fs::read_to_string(
        prototype_root().join("tests/queries/oracles/results-cache/wire-cache-topk.json"),
    )?)?;
    let mut latest: Vec<(i64, String)> = documents
        .into_iter()
        .map(|document| (document.timestamp, document.message))
        .collect();
    latest.sort_by(|left, right| right.cmp(left));
    let expected: String = latest.into_iter().map(|(_, message)| message).collect();
    assert_eq!(String::from_utf8(output.stdout)?, expected);
    Ok(())
}

#[test]
fn misconfigured_or_unreachable_outputs_map_to_exit_codes() -> anyhow::Result<()> {
    let work = tempfile::tempdir()?;
    let closed_port = {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.local_addr()?.port().to_string()
    };
    let missing_directory = work.path().join("missing/results.msgpack");
    let missing_directory = missing_directory.to_string_lossy().into_owned();
    let archive = fixture("search.ts");
    for (args, expected_exit) in [
        (vec!["--output", "file"], 1),
        (vec!["--output", "network", "--port", "1"], 1),
        (vec!["--output", "network", "--host", "127.0.0.1"], 1),
        (vec!["--output", "results-cache", "--collection", "c"], 1),
        (
            vec![
                "--output",
                "results-cache",
                "--uri",
                "mongodb://127.0.0.1:1/db",
            ],
            1,
        ),
        (
            vec![
                "--output",
                "results-cache",
                "--uri",
                "mongodb://127.0.0.1:1",
                "--collection",
                "c",
            ],
            1,
        ),
        (
            vec![
                "--output",
                "results-cache",
                "--uri",
                "mongodb://127.0.0.1:1/db",
                "--collection",
                "",
            ],
            1,
        ),
        (
            vec![
                "--output",
                "results-cache",
                "--uri",
                "mongodb://127.0.0.1:1/db",
                "--collection",
                "c",
                "--batch-size",
                "0",
            ],
            1,
        ),
        (vec!["--max-num-results", "0"], 1),
        (vec!["--output", "file", "--path", &missing_directory], 4),
        (
            vec![
                "--output",
                "network",
                "--host",
                "127.0.0.1",
                "--port",
                &closed_port,
            ],
            4,
        ),
        (
            vec![
                "--output",
                "results-cache",
                "--uri",
                "mongodb://127.0.0.1:1/db?serverSelectionTimeoutMS=200",
                "--collection",
                "c",
            ],
            if cfg!(feature = "results-cache") {
                4
            } else {
                1
            },
        ),
    ] {
        let mut full_args = vec!["-q", "level: WARN"];
        full_args.extend(&args);
        let output = search(&[&archive], &full_args);
        assert_eq!(
            output.status.code(),
            Some(expected_exit),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, Vec::<u8>::new(), "{args:?}");
    }
    Ok(())
}
