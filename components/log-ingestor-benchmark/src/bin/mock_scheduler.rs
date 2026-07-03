//! The mock scheduler binary.
//!
//! It stands in for the CLP compression scheduler: it polls `compression_jobs` for newly submitted
//! jobs, mocks the scheduler's metadata read traffic by fetching and sorting each job's ingested
//! object metadata, and then marks the job succeeded. This lets the ingestor's fire-and-forget
//! completion path advance the ingested-object metadata to its terminal state.

use std::{
    collections::HashSet,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Context as _;
use clap::Parser;
use clp_rust_utils::{database::mysql::create_clp_db_mysql_pool, serde::yaml};
use log_ingestor_benchmark::{
    SchedulerConfig,
    build_db_config,
    init_logging,
    read_db_credentials,
    wait_for_termination_signal,
};
use sqlx::MySqlPool;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The compression-job status code for a pending job awaiting dispatch.
const STATUS_PENDING: i32 = 0;

/// The compression-job status code for a successfully completed job.
const STATUS_SUCCEEDED: i32 = 2;

/// The interval between database connection retries.
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// Extra database connections held above `max_concurrent_jobs` for the polling query itself.
const EXTRA_DB_CONNECTIONS: u32 = 8;

/// Command-line arguments for the mock scheduler.
#[derive(Parser)]
#[command(version, about = "Mock compression scheduler for the CLP log-ingestor benchmark.")]
struct Args {
    /// Path to the benchmark scheduler config file.
    #[arg(long)]
    config: String,
}

/// Creates the CLP database connection pool, retrying indefinitely until the database is reachable.
///
/// # Returns
///
/// A connected [`MySqlPool`].
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`read_db_credentials`]'s return values on failure.
async fn connect_pool_with_retry(config: &SchedulerConfig) -> anyhow::Result<MySqlPool> {
    let db_config = build_db_config(&config.database);
    let credentials = read_db_credentials()?;
    let max_connections = u32::try_from(config.max_concurrent_jobs)
        .unwrap_or(u32::MAX)
        .saturating_add(EXTRA_DB_CONNECTIONS);
    loop {
        match create_clp_db_mysql_pool(&db_config, &credentials.database, max_connections).await {
            Ok(pool) => return Ok(pool),
            Err(error) => {
                tracing::warn!(
                    error = ? error,
                    retry_interval_sec = CONNECT_RETRY_INTERVAL.as_secs_f64(),
                    "Failed to connect to CLP DB; retrying."
                );
                tokio::time::sleep(CONNECT_RETRY_INTERVAL).await;
            }
        }
    }
}

/// Processes a single compression job: mocks the scheduler's metadata read by fetching and sorting
/// the job's ingested object metadata, optionally waits for the simulated compression duration, and
/// then marks the job succeeded. The job ID is removed from `in_flight` when processing completes.
///
/// # Panics
///
/// Panics if the in-flight-set mutex is poisoned.
async fn process_compression_job(
    pool: MySqlPool,
    job_id: i32,
    simulated_compression_duration: Duration,
    in_flight: Arc<Mutex<HashSet<i32>>>,
    permit: OwnedSemaphorePermit,
) {
    const READ_METADATA_QUERY: &str = "SELECT `id`, `key`, `size` FROM \
         `ingested_s3_object_metadata` WHERE `compression_job_id` = ? ORDER BY `key`;";
    const MARK_SUCCEEDED_QUERY: &str = "UPDATE `compression_jobs` SET `status` = ?, \
         `start_time` = NOW(3), `duration` = ? WHERE `id` = ?;";

    let started = Instant::now();
    match sqlx::query_as::<_, (u64, String, u64)>(READ_METADATA_QUERY)
        .bind(job_id)
        .fetch_all(&pool)
        .await
    {
        Ok(rows) => {
            if !simulated_compression_duration.is_zero() {
                tokio::time::sleep(simulated_compression_duration).await;
            }
            let duration_sec = started.elapsed().as_secs_f64();
            match sqlx::query(MARK_SUCCEEDED_QUERY)
                .bind(STATUS_SUCCEEDED)
                .bind(duration_sec)
                .bind(job_id)
                .execute(&pool)
                .await
            {
                Ok(_) => tracing::info!(
                    job_id,
                    num_metadata = rows.len(),
                    duration_sec,
                    "Compression job marked succeeded."
                ),
                Err(error) => tracing::warn!(
                    error = ? error,
                    job_id,
                    "Failed to mark compression job succeeded."
                ),
            }
        }
        Err(error) => tracing::warn!(
            error = ? error,
            job_id,
            "Failed to read ingested metadata for compression job."
        ),
    }

    in_flight
        .lock()
        .expect("in-flight set mutex is poisoned")
        .remove(&job_id);
    drop(permit);
}

/// Fetches the IDs of all pending compression jobs.
///
/// # Returns
///
/// The IDs of every job currently in the pending state.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`sqlx::query::Query::fetch_all`]'s return values on failure.
async fn fetch_pending_job_ids(pool: &MySqlPool) -> anyhow::Result<Vec<i32>> {
    const QUERY: &str = "SELECT `id` FROM `compression_jobs` WHERE `status` = ?;";
    let ids = sqlx::query_scalar::<_, i32>(QUERY)
        .bind(STATUS_PENDING)
        .fetch_all(pool)
        .await?;
    Ok(ids)
}

/// Dispatches every not-yet-in-flight pending job to a bounded worker task.
///
/// # Panics
///
/// Panics if the in-flight-set mutex is poisoned, or if the job semaphore has been closed.
async fn dispatch_pending_jobs(
    pool: &MySqlPool,
    job_ids: Vec<i32>,
    simulated_compression_duration: Duration,
    semaphore: &Arc<Semaphore>,
    in_flight: &Arc<Mutex<HashSet<i32>>>,
) {
    for job_id in job_ids {
        {
            let mut guard = in_flight.lock().expect("in-flight set mutex is poisoned");
            if !guard.insert(job_id) {
                continue;
            }
        }
        let permit = Arc::clone(semaphore)
            .acquire_owned()
            .await
            .expect("semaphore is never closed");
        tokio::spawn(process_compression_job(
            pool.clone(),
            job_id,
            simulated_compression_duration,
            Arc::clone(in_flight),
            permit,
        ));
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    init_logging();

    let config: SchedulerConfig = yaml::from_path(Path::new(&args.config))
        .with_context(|| format!("Failed to load config file {}", args.config))?;

    tracing::info!(
        poll_interval_ms = config.poll_interval_ms,
        max_concurrent_jobs = config.max_concurrent_jobs,
        "Connecting to CLP DB."
    );
    let pool = connect_pool_with_retry(&config).await?;
    tracing::info!("Connected to CLP DB; polling for compression jobs.");

    let poll_interval = Duration::from_millis(config.poll_interval_ms);
    let simulated_compression_duration =
        Duration::from_millis(config.simulated_compression_duration_ms);
    let semaphore = Arc::new(Semaphore::new(config.max_concurrent_jobs.max(1)));
    let in_flight: Arc<Mutex<HashSet<i32>>> = Arc::new(Mutex::new(HashSet::new()));

    loop {
        tokio::select! {
            () = wait_for_termination_signal() => {
                tracing::info!("Shutdown requested; stopping mock scheduler.");
                break;
            }
            () = tokio::time::sleep(poll_interval) => {}
        }

        match fetch_pending_job_ids(&pool).await {
            Ok(job_ids) if !job_ids.is_empty() => {
                dispatch_pending_jobs(
                    &pool,
                    job_ids,
                    simulated_compression_duration,
                    &semaphore,
                    &in_flight,
                )
                .await;
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(error = ? error, "Failed to poll for pending compression jobs.");
            }
        }
    }

    Ok(())
}
