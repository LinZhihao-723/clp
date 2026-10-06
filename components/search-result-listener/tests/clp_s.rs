//! Tests that drive a [`ResultListener`] with a real `clp-s` binary, whose path is read from the
//! `CLP_S_BINARY` environment variable. The tests are skipped when the variable isn't set.

mod common;

use std::fmt::Write;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use clp_rust_utils::job_config::NetworkOutput;
use clp_rust_utils::job_config::QueryJobStatus;
use clp_rust_utils::types::ArchiveId;
use common::FakeJobStatusSource;
use futures::StreamExt;
use search_result_listener::ListenerConfig;
use search_result_listener::ResultListener;
use search_result_listener::SessionConfig;
use search_result_listener::SessionStats;
use tokio::process::Command;
use uuid::Uuid;

const NUM_RECORDS: u64 = 1000;
const QUERY: &str = "*Transmitted*";
const TIMEOUT: Duration = Duration::from_secs(60);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a clp-s binary; set `CLP_S_BINARY` to its path"]
async fn clp_s_attempts_of_one_task_emit_each_result_once() -> anyhow::Result<()> {
    // CI runs the ignored tests without building clp-s.
    let Some(clp_s) = std::env::var_os("CLP_S_BINARY").map(PathBuf::from) else {
        eprintln!("Skipping the test since `CLP_S_BINARY` isn't set.");
        return Ok(());
    };
    let work_dir = std::env::temp_dir().join(format!("search-result-listener-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&work_dir)?;
    let result = run_attempts(&clp_s, &work_dir).await;
    std::fs::remove_dir_all(&work_dir)?;
    result
}

/// Compresses a small log file into one archive, then streams the archive's search results to a
/// listener from overlapping and sequential attempts of the same task.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if:
///   * clp-s's own output has no results.
///   * The listener's results or statistics differ from clp-s's own output.
/// * Forwards [`compress`]'s return values on failure.
/// * Forwards [`str::parse`]'s return values on failure.
/// * Forwards [`search_to_stdout`]'s return values on failure.
/// * Forwards [`ResultListener::bind`]'s return values on failure.
/// * Forwards [`search_to_listener`]'s return values on failure.
/// * Forwards [`tokio::time::timeout`]'s return values on failure.
/// * Forwards [`search_result_listener::OutcomeFuture`]'s return values on failure.
/// * Forwards [`serde_json::from_str`]'s return values on failure.
/// * Forwards [`u64::try_from`]'s return values on failure.
async fn run_attempts(clp_s: &Path, work_dir: &Path) -> anyhow::Result<()> {
    let archives_dir = work_dir.join("archives");
    let archive_name = compress(clp_s, work_dir, &archives_dir).await?;
    let archive_id = archive_name.parse::<ArchiveId>()?;
    let expected_messages = search_to_stdout(clp_s, &archives_dir, &archive_name).await?;
    anyhow::ensure!(
        !expected_messages.is_empty(),
        "the query should match records"
    );

    let listener = ResultListener::bind(ListenerConfig {
        bind_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        ..ListenerConfig::default()
    })
    .await?;
    let session = listener.open_session(SessionConfig::default());
    let network_output = session.network_output().clone();
    let status_source = FakeJobStatusSource::new();
    let (results, outcome) = session.run(1, status_source.clone());

    let search = |query: &'static str, task_index: u64| {
        search_to_listener(
            clp_s,
            &archives_dir,
            &archive_name,
            query,
            &network_output,
            task_index,
        )
    };
    let (first_attempt, second_attempt, empty_task) =
        tokio::join!(search(QUERY, 0), search(QUERY, 0), search("msg: absent", 1));
    first_attempt?;
    second_attempt?;
    empty_task?;
    search(QUERY, 0).await?;
    status_source.set(Some(QueryJobStatus::Succeeded));

    let results = tokio::time::timeout(TIMEOUT, results.collect::<Vec<_>>()).await?;
    let outcome = tokio::time::timeout(TIMEOUT, outcome).await??;
    let mut messages = Vec::with_capacity(results.len());
    for result in results {
        anyhow::ensure!(
            result.archive_id == archive_id,
            "wrong archive ID: {result:?}"
        );
        let record: serde_json::Value = serde_json::from_str(&result.message)?;
        anyhow::ensure!(
            record["ts"].as_i64() == Some(result.timestamp),
            "the timestamp should be the record's `ts`: {result:?}"
        );
        messages.push(result.message);
    }
    messages.sort_unstable();
    let num_expected = u64::try_from(expected_messages.len())?;
    anyhow::ensure!(
        messages == expected_messages,
        "the listener should emit clp-s's results exactly once"
    );
    anyhow::ensure!(
        outcome.stats
            == SessionStats {
                num_results_emitted: num_expected,
                num_duplicates_dropped: 2 * num_expected,
                num_protocol_errors: 0,
            },
        "unexpected statistics: {:?}",
        outcome.stats
    );
    Ok(())
}

/// Writes a log file of [`NUM_RECORDS`] JSON records to `work_dir` and compresses it into
/// `archives_dir`.
///
/// # Returns
///
/// The name of the archive's directory on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if the compression doesn't produce exactly one archive.
/// * Forwards [`std::fmt::Write::write_fmt`]'s return values on failure.
/// * Forwards [`std::fs::write`]'s return values on failure.
/// * Forwards [`run_clp_s`]'s return values on failure.
/// * Forwards [`std::fs::read_dir`]'s return values on failure.
/// * Forwards [`std::fs::ReadDir::next`]'s return values on failure.
async fn compress(clp_s: &Path, work_dir: &Path, archives_dir: &Path) -> anyhow::Result<String> {
    let mut records = String::new();
    for record_index in 0..NUM_RECORDS {
        let action = if 0 == record_index % 7 {
            "Transmitted"
        } else {
            "Received"
        };
        writeln!(
            records,
            "{{\"ts\":{},\"msg\":\"{action} block {record_index}\",\"seq\":{record_index}}}",
            1_700_000_000_000 + record_index * 1000
        )?;
    }
    let input_path = work_dir.join("input.jsonl");
    std::fs::write(&input_path, records)?;
    run_clp_s(
        clp_s,
        &[
            "c".as_ref(),
            "--timestamp-key".as_ref(),
            "ts".as_ref(),
            archives_dir.as_os_str(),
            input_path.as_os_str(),
        ],
    )
    .await?;

    let mut archive_names = std::fs::read_dir(archives_dir)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    anyhow::ensure!(
        1 == archive_names.len(),
        "expected one archive, found {archive_names:?}"
    );
    Ok(archive_names.remove(0))
}

/// Searches the archive `archive_name` for [`QUERY`] with clp-s's default output handler.
///
/// # Returns
///
/// The sorted messages clp-s prints on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`run_clp_s`]'s return values on failure.
async fn search_to_stdout(
    clp_s: &Path,
    archives_dir: &Path,
    archive_name: &str,
) -> anyhow::Result<Vec<String>> {
    let output = run_clp_s(
        clp_s,
        &[
            "s".as_ref(),
            archives_dir.as_os_str(),
            "--archive-id".as_ref(),
            archive_name.as_ref(),
            QUERY.as_ref(),
        ],
    )
    .await?;
    let mut messages: Vec<String> = output
        .split_inclusive('\n')
        .map(ToOwned::to_owned)
        .collect();
    messages.sort_unstable();
    Ok(messages)
}

/// Runs an attempt of task `task_index`, which searches the archive `archive_name` for `query` and
/// streams the results to `network_output`.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`run_clp_s`]'s return values on failure.
async fn search_to_listener(
    clp_s: &Path,
    archives_dir: &Path,
    archive_name: &str,
    query: &str,
    network_output: &NetworkOutput,
    task_index: u64,
) -> anyhow::Result<()> {
    run_clp_s(
        clp_s,
        &[
            "s".as_ref(),
            archives_dir.as_os_str(),
            "--archive-id".as_ref(),
            archive_name.as_ref(),
            query.as_ref(),
            "network".as_ref(),
            "--host".as_ref(),
            network_output.host.as_str().as_ref(),
            "--port".as_ref(),
            network_output.port.to_string().as_ref(),
            "--session-token".as_ref(),
            network_output.session_token.to_string().as_ref(),
            "--task-index".as_ref(),
            task_index.to_string().as_ref(),
        ],
    )
    .await?;
    Ok(())
}

/// Runs clp-s with `args`.
///
/// # Returns
///
/// clp-s's stdout on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if clp-s exits unsuccessfully or prints non-UTF-8 output.
/// * Forwards [`Command::output`]'s return values on failure.
async fn run_clp_s(clp_s: &Path, args: &[&std::ffi::OsStr]) -> anyhow::Result<String> {
    let output = Command::new(clp_s).args(args).output().await?;
    anyhow::ensure!(
        output.status.success(),
        "clp-s {args:?} failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}
