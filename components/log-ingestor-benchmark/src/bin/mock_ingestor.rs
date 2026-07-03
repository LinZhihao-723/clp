//! The mock ingestor binary.
//!
//! It creates a configurable number of ingestion jobs and drives synthetic object metadata through
//! the real [`log_ingestor`] ingestion state, buffer, listener, and compression-job-submitter
//! pipeline at a throttled rate, exercising the true CLP database persistence path without SQS.

use std::{path::Path, time::Duration};

use anyhow::Context as _;
use clap::Parser;
use clp_rust_utils::serde::yaml;
use log_ingestor::{
    ingestion_job::{IngestionJobState, SqsListenerState},
    ingestion_job_manager::ClpIngestionState,
    telemetry::snapshot_db_call_metrics,
};
use log_ingestor_benchmark::{
    IngestorConfig,
    WorkloadConfig,
    Xorshift64,
    build_clp_config,
    build_ingestion_job_config,
    connect_with_retry,
    format_db_call_metrics,
    generate_batch,
    init_logging,
    read_db_credentials,
    wait_for_termination_signal,
};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// The interval between database connection retries.
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// Command-line arguments for the mock ingestor.
#[derive(Parser)]
#[command(version, about = "Mock ingestor for the CLP log-ingestor benchmark.")]
struct Args {
    /// Path to the benchmark ingestor config file.
    #[arg(long)]
    config: String,
}

/// Computes the pacing delay applied per generated object so that each task hits its share of the
/// configured per-job ingestion rate.
///
/// # Returns
///
/// The per-object delay, or `None` when throttling is disabled (a zero rate or zero task count).
fn per_object_delay(workload: &WorkloadConfig) -> Option<Duration> {
    if workload.entries_per_minute_per_job == 0 || workload.tasks_per_job == 0 {
        return None;
    }
    let per_task_rate = f64::from(workload.entries_per_minute_per_job)
        / f64::from(workload.tasks_per_job);
    if per_task_rate <= 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(60.0 / per_task_rate))
}

/// Runs a single ingestion task, repeatedly generating and ingesting throttled batches of synthetic
/// object metadata until `token` is cancelled. The generator is seeded solely from the job and task
/// indices so the workload is reproducible.
///
/// # Panics
///
/// Panics if a generated batch size does not fit in `u32`, which cannot happen because batch sizes
/// are bounded by the configured maximum.
async fn run_ingest_task(
    state: ClpIngestionState,
    job_index: u32,
    task_index: u32,
    workload: WorkloadConfig,
    per_object_delay: Option<Duration>,
    token: CancellationToken,
) {
    let seed = (u64::from(job_index) << 32) | u64::from(task_index);
    let mut rng = Xorshift64::new(seed);
    let mut next_object_index: u64 = 0;

    while !token.is_cancelled() {
        let batch_size = u32::try_from(rng.next_in_range(
            u64::from(workload.batch_size_min),
            u64::from(workload.batch_size_max),
        ))
        .expect("batch size fits in u32");
        let batch = generate_batch(
            &mut rng,
            job_index,
            task_index,
            &mut next_object_index,
            batch_size,
            workload.object_size_bytes_mean,
        );

        if let Err(error) = SqsListenerState::ingest(&state, batch).await {
            tracing::warn!(
                error = ? error,
                job_index,
                task_index,
                "Failed to ingest a batch of object metadata."
            );
        }

        let Some(delay) = per_object_delay else {
            continue;
        };
        tokio::select! {
            () = token.cancelled() => break,
            () = tokio::time::sleep(delay * batch_size) => {}
        }
    }
}

/// Periodically logs a database-call metrics report until `token` is cancelled. When `interval_sec`
/// is zero, periodic reporting is disabled and the task simply waits for cancellation.
async fn run_metrics_reporter(interval_sec: u64, token: CancellationToken) {
    if interval_sec == 0 {
        token.cancelled().await;
        return;
    }
    let interval = Duration::from_secs(interval_sec);
    loop {
        tokio::select! {
            () = token.cancelled() => break,
            () = tokio::time::sleep(interval) => {
                tracing::info!("{}", format_db_call_metrics(&snapshot_db_call_metrics()));
            }
        }
    }
}

/// Waits for a termination signal, or for the configured run duration to elapse when it is
/// non-zero.
async fn wait_for_shutdown(run_duration_sec: u64) {
    if run_duration_sec == 0 {
        wait_for_termination_signal().await;
        return;
    }
    tokio::select! {
        () = wait_for_termination_signal() => {}
        () = tokio::time::sleep(Duration::from_secs(run_duration_sec)) => {
            tracing::info!(run_duration_sec, "Configured run duration elapsed.");
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    init_logging();

    let config: IngestorConfig = yaml::from_path(Path::new(&args.config))
        .with_context(|| format!("Failed to load config file {}", args.config))?;
    let credentials = read_db_credentials()?;
    let clp_config = build_clp_config(&config.database)?;

    tracing::info!(
        num_jobs = config.workload.num_jobs,
        tasks_per_job = config.workload.tasks_per_job,
        "Connecting to CLP DB."
    );
    let connector = connect_with_retry(&clp_config, &credentials, CONNECT_RETRY_INTERVAL).await;
    tracing::info!("Connected to CLP DB.");

    let token = CancellationToken::new();
    let reporter = tokio::spawn(run_metrics_reporter(
        config.metrics_report_interval_sec,
        token.clone(),
    ));

    let delay = per_object_delay(&config.workload);
    let mut contexts = Vec::new();
    let mut tasks: JoinSet<()> = JoinSet::new();

    for job_index in 0..config.workload.num_jobs {
        let job_config = build_ingestion_job_config(job_index, &config.buffer)?;
        let context = connector
            .create_ingestion_job(job_config)
            .await
            .with_context(|| format!("Failed to create ingestion job {job_index}"))?;
        IngestionJobState::start(&context.get_ingestion_state())
            .await
            .with_context(|| format!("Failed to start ingestion job {job_index}"))?;

        for task_index in 0..config.workload.tasks_per_job {
            let state = context.get_ingestion_state();
            let workload = config.workload.clone();
            let task_token = token.clone();
            tasks.spawn(run_ingest_task(
                state,
                job_index,
                task_index,
                workload,
                delay,
                task_token,
            ));
        }
        contexts.push(context);
    }
    tracing::info!(num_jobs = contexts.len(), "All ingestion jobs started.");

    wait_for_shutdown(config.workload.run_duration_sec).await;
    tracing::info!("Shutdown requested; stopping ingestion tasks.");
    token.cancel();
    while let Some(joined) = tasks.join_next().await {
        if let Err(error) = joined {
            tracing::warn!(error = ? error, "An ingestion task terminated abnormally.");
        }
    }
    if let Err(error) = reporter.await {
        tracing::warn!(error = ? error, "The metrics reporter task terminated abnormally.");
    }

    for context in contexts {
        let job_id = context.get_job_id();
        match context.shutdown().await {
            Ok(_) => tracing::info!(job_id, "Ingestion job shut down."),
            Err(error) => {
                tracing::warn!(error = ? error, job_id, "Failed to shut down ingestion job.");
            }
        }
    }
    tracing::info!(
        "final DB-call metrics: {}",
        format_db_call_metrics(&snapshot_db_call_metrics())
    );
    tracing::info!("Mock ingestor stopped.");

    Ok(())
}
