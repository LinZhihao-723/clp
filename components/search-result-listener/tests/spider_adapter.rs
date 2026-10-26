//! Tests that drive a [`ResultListener`] with the Spider adapter's search tasks over the clp-s
//! reader's C++-built fixture archives: in-process with the Rust reader, and with a real `clp-s`
//! binary whose path is read from the `CLP_S_BINARY` environment variable. The tests are skipped
//! when the variable isn't set.

mod common;

use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use clp_rust_utils::clp_config::package::config::ArchiveOutput;
use clp_rust_utils::clp_config::package::config::ArchiveOutputStorage;
use clp_rust_utils::clp_config::package::config::Package;
use clp_rust_utils::clp_config::package::config::SpiderTaskExecutorConfig;
use clp_rust_utils::clp_config::package::config::StorageEngine;
use clp_rust_utils::job_config::QueryJobStatus;
use clp_rust_utils::task_io::query::ArchiveToSearch;
use clp_rust_utils::task_io::query::ClpSQueryOption;
use clp_rust_utils::task_io::query::OutputHandle;
use clp_rust_utils::types::ArchiveId;
use clp_rust_utils::types::non_empty_string::ExpectedNonEmpty;
use clp_s_spider_adapter::AdapterConfig;
use clp_s_spider_adapter::AdapterResources;
use common::FakeJobStatusSource;
use futures::StreamExt;
use non_empty_string::NonEmptyString;
use search_result_listener::ListenerConfig;
use search_result_listener::ResultListener;
use search_result_listener::SearchResultEncoding;
use search_result_listener::SessionConfig;
use search_result_listener::SessionStats;
use spider_core::types::id::JobId;
use spider_core::types::id::ResourceGroupId;
use spider_core::types::id::TaskId;
use spider_tdl::TaskContext;
use uuid::Uuid;

const DATASET: &str = "default";
const TIMEOUT: Duration = Duration::from_mins(1);

/// The fixture archives the tasks search, one task each, with the query of every task.
const CASES: [(&str, &str); 6] = [
    ("logs.ts", "*"),
    ("logs.ts-tables4k", "level: ERROR"),
    ("many-schemas.default", "*"),
    ("nested.default", "*"),
    ("search.ts", "level: WARN OR level: ERROR"),
    ("strings.default", "*"),
];

/// The engine a search task searches with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Engine {
    /// The Spider adapter's in-process search with the Rust reader.
    Rust,

    /// The `clp-s` subprocess.
    ClpS,
}

/// One result as the listener emits it: `(archive_id, timestamp, message)`.
type EmittedResult = (String, i64, String);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a clp-s binary; set `CLP_S_BINARY` to its path"]
async fn rust_and_clp_s_tasks_emit_the_same_results() -> anyhow::Result<()> {
    // CI runs the ignored tests without building clp-s.
    let Some(clp_s) = std::env::var_os("CLP_S_BINARY").map(PathBuf::from) else {
        eprintln!("Skipping the test since `CLP_S_BINARY` isn't set.");
        return Ok(());
    };
    let clp_home = std::env::temp_dir().join(format!("search-result-listener-{}", Uuid::new_v4()));
    let result = compare_engines(&clp_s, &clp_home).await;
    std::fs::remove_dir_all(&clp_home)?;
    result
}

/// Stages the fixture archives in a CLP home whose `bin/clp-s` is `clp_s`, then runs one search
/// task per archive in sessions of a listener: twice with the in-process search, and once with
/// the `clp-s` subprocess.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if:
///   * The `clp-s` subprocess emits no results.
///   * The two engines emit different results.
///   * The in-process attempts don't stream every archive's results in the same order.
/// * Forwards [`std::fs::create_dir_all`]'s return values on failure.
/// * Forwards [`std::os::unix::fs::symlink`]'s return values on failure.
/// * Forwards [`ResultListener::bind`]'s return values on failure.
/// * Forwards [`AdapterResources::init`]'s return values on failure.
/// * Forwards [`run_session`]'s return values on failure.
async fn compare_engines(clp_s: &Path, clp_home: &Path) -> anyhow::Result<()> {
    let bin = clp_home.join("bin");
    std::fs::create_dir_all(&bin)?;
    std::os::unix::fs::symlink(clp_s, bin.join("clp-s"))?;
    let dataset_dir = clp_home.join("archives").join(DATASET);
    std::fs::create_dir_all(&dataset_dir)?;
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../core/rust/tests/fixtures/archives")
        .canonicalize()?;
    let mut archive_ids = Vec::with_capacity(CASES.len());
    for (name, _) in CASES {
        let archive_id: ArchiveId = Uuid::new_v4().to_string().parse()?;
        std::os::unix::fs::symlink(
            fixtures.join(format!("{name}.clps")),
            dataset_dir.join(archive_id.to_string()),
        )?;
        archive_ids.push(archive_id);
    }

    AdapterResources::init(
        tokio::runtime::Handle::current(),
        clp_s_reader::ReaderContext::builder().build()?,
        AdapterConfig {
            clp_home: clp_home.to_path_buf(),
            base_admission_timeout_sec: 60,
        },
    )?;
    let listener = ResultListener::bind(ListenerConfig {
        bind_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        ..ListenerConfig::default()
    })
    .await?;

    let (cpp_results, cpp_stats) = run_session(&listener, &archive_ids, &[Engine::ClpS]).await?;
    let (first_results, first_stats) =
        run_session(&listener, &archive_ids, &[Engine::Rust, Engine::Rust]).await?;
    let (second_results, _) = run_session(&listener, &archive_ids, &[Engine::Rust]).await?;
    anyhow::ensure!(!cpp_results.is_empty(), "the queries should match records");

    let num_results = u64::try_from(cpp_results.len())?;
    anyhow::ensure!(
        cpp_stats
            == SessionStats {
                num_results_emitted: num_results,
                ..SessionStats::default()
            },
        "unexpected clp-s statistics: {cpp_stats:?}"
    );
    anyhow::ensure!(
        first_stats
            == SessionStats {
                num_results_emitted: num_results,
                num_duplicates_dropped: num_results,
                ..SessionStats::default()
            },
        "unexpected in-process statistics: {first_stats:?}"
    );
    for archive_id in &archive_ids {
        let archive_id = archive_id.to_string();
        let of_archive = |results: &[EmittedResult]| -> Vec<EmittedResult> {
            results
                .iter()
                .filter(|result| result.0 == archive_id)
                .cloned()
                .collect()
        };
        anyhow::ensure!(
            of_archive(&first_results) == of_archive(&second_results),
            "the in-process attempts should stream archive {archive_id}'s results in the same \
             order"
        );
    }
    anyhow::ensure!(
        sorted(first_results) == sorted(cpp_results),
        "the engines should emit the same results"
    );
    Ok(())
}

/// Runs one search task per archive of `archive_ids` in a new session of `listener`, once per
/// attempt of `attempts`, one task after another.
///
/// # Returns
///
/// A tuple on success, containing:
///
/// * The results the session emitted, in emission order.
/// * The session's statistics.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if a search task fails or panics.
/// * Forwards [`tokio::time::timeout`]'s return values on failure.
/// * Forwards [`search_result_listener::OutcomeFuture`]'s return values on failure.
async fn run_session(
    listener: &ResultListener<SearchResultEncoding>,
    archive_ids: &[ArchiveId],
    attempts: &[Engine],
) -> anyhow::Result<(Vec<EmittedResult>, SessionStats)> {
    let session = listener.open_session(SessionConfig::default(), SearchResultEncoding);
    let output_handle = OutputHandle::from(session.network_output().clone());
    let status_source = FakeJobStatusSource::new();
    let (results, outcome) = session.run(1, status_source.clone());
    let collector = tokio::spawn(results.concat());
    for (task_index, archive_id) in (0_u64..).zip(archive_ids) {
        for engine in attempts {
            let (archive_id, engine, output_handle) = (*archive_id, *engine, output_handle.clone());
            tokio::task::spawn_blocking(move || {
                search(engine, archive_id, task_index, &output_handle)
            })
            .await??;
        }
    }
    status_source.set(Some(QueryJobStatus::Succeeded));
    let results = tokio::time::timeout(TIMEOUT, collector).await??;
    let outcome = tokio::time::timeout(TIMEOUT, outcome).await??;
    Ok((
        results
            .into_iter()
            .map(|result| {
                (
                    result.archive_id.to_string(),
                    result.timestamp,
                    result.message,
                )
            })
            .collect(),
        outcome.stats,
    ))
}

/// Runs search task `task_index` over the archive `archive_id` with `engine`, streaming to
/// `output_handle`.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`TaskContext::new`]'s return values on failure.
/// * Forwards [`clp_s_spider_adapter::search`]'s return values on failure.
/// * Forwards [`clp_s_spider_adapter::search_with_clp_s_subprocess`]'s return values on failure.
fn search(
    engine: Engine,
    archive_id: ArchiveId,
    task_index: u64,
    output_handle: &OutputHandle,
) -> anyhow::Result<()> {
    let ctx = TaskContext::new(
        JobId::random(),
        TaskId::Index(0),
        1,
        ResourceGroupId::random(),
        None,
    )?;
    let config = SpiderTaskExecutorConfig {
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
    };
    let clp_s_query_option = ClpSQueryOption {
        query_string: NonEmptyString::new(CASES[usize::try_from(task_index)?].1.to_owned())
            .map_err(|_| anyhow::anyhow!("the query is empty"))?,
        max_num_results: None,
        begin_timestamp_millisecs: None,
        end_timestamp_millisecs: None,
        ignore_case: false,
    };
    let archive = ArchiveToSearch {
        id: archive_id,
        dataset: Some(NonEmptyString::from_static_str(DATASET)),
        task_index,
    };
    let task = match engine {
        Engine::Rust => clp_s_spider_adapter::search,
        Engine::ClpS => clp_s_spider_adapter::search_with_clp_s_subprocess,
    };
    task(
        &ctx,
        &config,
        1,
        &clp_s_query_option,
        &archive,
        output_handle,
    )
}

/// # Returns
///
/// `results`, sorted.
fn sorted(mut results: Vec<EmittedResult>) -> Vec<EmittedResult> {
    results.sort_unstable();
    results
}
