//! The `clp-streaming-search` executable, which searches the compressed logs through the query
//! coordinator and prints the results that the search tasks stream back.

use std::io::IsTerminal;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use clp_rust_utils::clp_config::package;
use clp_rust_utils::database::mysql::create_clp_db_mysql_pool;
use clp_rust_utils::database::mysql::submit_query_job;
use clp_rust_utils::dataset::CLP_DEFAULT_DATASET_NAME;
use clp_rust_utils::job_config::QueryJobStatus;
use clp_rust_utils::job_config::SearchJobConfig;
use clp_rust_utils::serde::yaml;
use futures::StreamExt;
use non_empty_string::NonEmptyString;
use search_result_listener::ListenerConfig;
use search_result_listener::MariaDbJobStatusSource;
use search_result_listener::ResultListener;
use search_result_listener::ResultStream;
use search_result_listener::SearchResult;
use search_result_listener::SessionConfig;
use search_result_listener::SessionOutcome;
use tokio::signal::unix::SignalKind;
use tokio::signal::unix::signal;
use tracing_subscriber::EnvFilter;

const DATABASE_CONNECTION_POOL_SIZE: u32 = 1;
const JOB_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const SIGINT_EXIT_CODE: u8 = 130;
const SIGTERM_EXIT_CODE: u8 = 143;

/// Command-line arguments for `clp-streaming-search`.
#[derive(Debug, Parser)]
#[command(about = "Search the compressed logs and stream the results from the search tasks.")]
struct Cli {
    /// Path to the configuration file.
    #[arg(short, long, value_name = "PATH")]
    config: PathBuf,

    /// A dataset to search. Can be specified multiple times. Defaults to the `default` dataset.
    #[arg(long = "dataset", value_name = "NAME")]
    datasets: Vec<String>,

    /// Time range filter lower-bound (inclusive) as milliseconds from the UNIX epoch.
    #[arg(long, value_name = "EPOCH_MS", allow_negative_numbers = true)]
    begin_time: Option<i64>,

    /// Time range filter upper-bound (inclusive) as milliseconds from the UNIX epoch.
    #[arg(long, value_name = "EPOCH_MS", allow_negative_numbers = true)]
    end_time: Option<i64>,

    /// Ignore case distinctions between values in the query and the compressed data.
    #[arg(long)]
    ignore_case: bool,

    /// Output the search results as raw logs.
    #[arg(long)]
    raw: bool,

    /// Host the search tasks connect to in order to stream results to this tool. Defaults to the
    /// first non-loopback IPv4 address of this machine.
    #[arg(long, value_name = "HOST", value_parser = parse_non_empty)]
    advertised_host: Option<NonEmptyString>,

    /// Wildcard query.
    wildcard_query: String,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Cli::parse();
    set_up_logging();

    let signals = signal(SignalKind::interrupt())
        .and_then(|sigint| Ok((sigint, signal(SignalKind::terminate())?)));
    let (mut sigint, mut sigterm) = match signals {
        Ok(signals) => signals,
        Err(e) => {
            tracing::error!(error = % e, "Failed to listen for SIGINT and SIGTERM.");
            return ExitCode::FAILURE;
        }
    };

    tokio::select! {
        result = search(args) => match result {
            Ok(QueryJobStatus::Succeeded) => ExitCode::SUCCESS,
            Ok(_) | Err(_) => ExitCode::FAILURE,
        },
        _ = sigint.recv() => {
            tracing::warn!("Interrupted; the submitted query job, if any, keeps running.");
            ExitCode::from(SIGINT_EXIT_CODE)
        }
        _ = sigterm.recv() => {
            tracing::warn!("Terminated; the submitted query job, if any, keeps running.");
            ExitCode::from(SIGTERM_EXIT_CODE)
        }
    }
}

/// Submits a search job whose tasks stream their results to this process, prints the results as
/// they arrive, and waits for the job to terminate.
///
/// # Returns
///
/// The query job's terminal status on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if the begin time is later than the end time.
/// * Forwards [`yaml::from_path`]'s return values on failure.
/// * Forwards [`package::credentials::Database::from_env`]'s return values on failure.
/// * Forwards [`create_clp_db_mysql_pool`]'s return values on failure.
/// * Forwards [`ResultListener::bind`]'s return values on failure.
/// * Forwards [`submit_query_job`]'s return values on failure.
/// * Forwards [`print_results`]'s return values on failure.
/// * Forwards [`search_result_listener::OutcomeFuture`]'s return values on failure.
async fn search(args: Cli) -> anyhow::Result<QueryJobStatus> {
    if let (Some(begin_time), Some(end_time)) = (args.begin_time, args.end_time)
        && begin_time > end_time
    {
        tracing::error!(
            begin_time,
            end_time,
            "The begin time must not be later than the end time."
        );
        anyhow::bail!("the begin time is later than the end time");
    }

    let config: package::config::Config = yaml::from_path(&args.config).inspect_err(|e| {
        tracing::error!(error = % e, "Failed to load the configuration file.");
    })?;
    let database_credentials = package::credentials::Database::from_env()?;
    let db_pool = create_clp_db_mysql_pool(
        &config.database,
        &database_credentials,
        DATABASE_CONNECTION_POOL_SIZE,
    )
    .await
    .inspect_err(|e| tracing::error!(error = % e, "Failed to create the database pool."))?;

    let listener = ResultListener::bind(ListenerConfig {
        advertised_host: args.advertised_host,
        ..ListenerConfig::default()
    })
    .await
    .inspect_err(|e| tracing::error!(error = % e, "Failed to listen for search results."))?;
    let session = listener.open_session(SessionConfig::default());

    let datasets = if args.datasets.is_empty() {
        vec![CLP_DEFAULT_DATASET_NAME.to_owned()]
    } else {
        args.datasets
    };
    let search_job_config = SearchJobConfig {
        datasets: Some(datasets),
        query_string: args.wildcard_query,
        max_num_results: 0,
        begin_timestamp: args.begin_time,
        end_timestamp: args.end_time,
        ignore_case: args.ignore_case,
        network_output: Some(session.network_output().clone()),
        ..SearchJobConfig::default()
    };
    let query_job_id = submit_query_job(&db_pool, &search_job_config)
        .await
        .inspect_err(|e| tracing::error!(error = % e, "Failed to submit the query job."))?;
    tracing::debug!(
        query_job_id,
        network_output = ? search_job_config.network_output,
        "Submitted the query job."
    );

    let (results, outcome) = session.run(
        query_job_id,
        MariaDbJobStatusSource::new(db_pool, JOB_STATUS_POLL_INTERVAL),
    );
    print_results(results, args.raw).await.inspect_err(|e| {
        tracing::error!(error = % e, "Failed to print the search results.");
    })?;
    let SessionOutcome { status, stats } = outcome.await.inspect_err(|e| {
        tracing::error!(
            query_job_id,
            error = % e,
            "Failed to wait for the query job to terminate."
        );
    })?;

    if QueryJobStatus::Succeeded == status {
        tracing::debug!(
            query_job_id,
            num_results_emitted = stats.num_results_emitted,
            num_duplicates_dropped = stats.num_duplicates_dropped,
            num_protocol_errors = stats.num_protocol_errors,
            "The query job succeeded."
        );
    } else {
        tracing::error!(
            query_job_id,
            status = ? status,
            num_results_emitted = stats.num_results_emitted,
            num_duplicates_dropped = stats.num_duplicates_dropped,
            num_protocol_errors = stats.num_protocol_errors,
            "The query job didn't succeed."
        );
    }
    Ok(status)
}

/// Initializes logging to stderr, since stdout carries the search results.
fn set_up_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_target(false)
        .init();
}

/// Prints each result to stdout as it arrives, writing each result with a single write.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`Write::write_all`]'s return values on failure.
/// * Forwards [`Write::flush`]'s return values on failure.
async fn print_results(mut results: ResultStream, raw: bool) -> std::io::Result<()> {
    let mut stdout = std::io::stdout();
    let mut line = Vec::new();
    while let Some(result) = results.next().await {
        line.clear();
        format_result(&mut line, &result, raw);
        stdout.write_all(&line)?;
        stdout.flush()?;
    }
    Ok(())
}

/// Appends `result` to `line`: only the message if `raw` is set, otherwise the message prefixed
/// with the result's archive ID and timestamp.
///
/// # Panics
///
/// Panics if writing to `line` fails, which writing to a `Vec` never does.
fn format_result(line: &mut Vec<u8>, result: &SearchResult, raw: bool) {
    if !raw {
        write!(line, "{} {}: ", result.archive_id, result.timestamp)
            .expect("writing to a `Vec` shouldn't fail");
    }
    line.extend_from_slice(result.message.as_bytes());
}

/// Parses a command-line value that must not be empty.
///
/// # Returns
///
/// The parsed value on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`String`] if `value` is empty.
fn parse_non_empty(value: &str) -> Result<NonEmptyString, String> {
    NonEmptyString::new(value.to_owned()).map_err(|_| "the value must not be empty".to_owned())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use clap::Parser;
    use clp_rust_utils::types::ArchiveId;
    use non_empty_string::NonEmptyString;
    use search_result_listener::SearchResult;

    use super::Cli;
    use super::format_result;

    const ARCHIVE_ID: &str = "018e90e5-8b2a-4a61-a2fc-cac799936caf";

    /// # Returns
    ///
    /// A result of the archive [`ARCHIVE_ID`].
    fn search_result() -> SearchResult {
        SearchResult {
            archive_id: ARCHIVE_ID.parse::<ArchiveId>().expect("valid archive UUID"),
            timestamp: 1_700_000_000_123,
            message: "{\"msg\":\"Transmitted block\"}\n".to_owned(),
        }
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn cli_parses_the_arguments_the_wrapper_passes() {
        let args = Cli::try_parse_from([
            "clp-streaming-search",
            "--config",
            "/opt/clp/etc/clp-config.yaml",
            "--dataset",
            "default",
            "--dataset",
            "other",
            "--begin-time",
            "-5",
            "--end-time",
            "1700000000000",
            "--ignore-case",
            "--raw",
            "--advertised-host",
            "10.0.0.7",
            "--",
            "-*Transmitted*",
        ])
        .expect("the arguments should parse");

        assert_eq!(args.datasets, ["default", "other"]);
        assert_eq!(args.begin_time, Some(-5));
        assert_eq!(args.end_time, Some(1_700_000_000_000));
        assert!(args.ignore_case);
        assert!(args.raw);
        assert_eq!(
            args.advertised_host.as_ref().map(NonEmptyString::as_str),
            Some("10.0.0.7")
        );
        assert_eq!(args.wildcard_query, "-*Transmitted*");
    }

    #[test]
    fn cli_defaults_to_no_datasets_and_detected_host() {
        let args = Cli::try_parse_from(["clp-streaming-search", "--config", "c.yaml", "*error*"])
            .expect("the arguments should parse");

        assert_eq!(args.datasets, [] as [String; 0]);
        assert_eq!(args.advertised_host, None);
        assert!(!args.raw);
    }

    #[test]
    fn cli_rejects_an_empty_advertised_host() {
        Cli::try_parse_from([
            "clp-streaming-search",
            "--config",
            "c.yaml",
            "--advertised-host",
            "",
            "*error*",
        ])
        .expect_err("an empty host should be rejected");
    }

    #[test]
    fn default_format_prefixes_the_archive_id_and_timestamp() {
        let mut line = Vec::new();
        format_result(&mut line, &search_result(), false);
        assert_eq!(
            String::from_utf8(line).expect("the line should be UTF-8"),
            format!("{ARCHIVE_ID} 1700000000123: {{\"msg\":\"Transmitted block\"}}\n")
        );
    }

    #[test]
    fn raw_format_prints_only_the_message() {
        let mut line = Vec::new();
        format_result(&mut line, &search_result(), true);
        assert_eq!(
            String::from_utf8(line).expect("the line should be UTF-8"),
            "{\"msg\":\"Transmitted block\"}\n"
        );
    }
}
