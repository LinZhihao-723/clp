//! Shared configuration, credential loading, logging setup, a deterministic pseudo-random number
//! generator, and synthetic-workload generation for the log-ingestor benchmark harness.
//!
//! The harness drives the real CLP database ingestion path (bypassing SQS) so that the two
//! binaries in this crate, together with a `MySQL` database, form an end-to-end benchmark of CLP
//! database ingestion.

use std::time::Duration;

use anyhow::Context as _;
use clp_rust_utils::{
    clp_config::package::{
        config::{ClpDbNames, Config as ClpConfig, Database as ClpDbConfig},
        credentials::{Credentials as ClpCredentials, Database as DbCredentials},
    },
    job_config::ingestion::s3::S3IngestionJobConfig,
    s3::ObjectMetadata,
};
use log_ingestor::{
    ingestion_job_manager::ClpDbIngestionConnector,
    telemetry::DbCallMetricsSnapshot,
};
use secrecy::SecretString;
use serde::Deserialize;
use tracing_subscriber::{EnvFilter, fmt};

/// The `spider` database name required by [`ClpDbNames`]. The benchmark never touches the Spider
/// database, but the field must be present to deserialize a [`ClpConfig`].
const SPIDER_DB_NAME: &str = "spider-db";

/// The environment variable holding the CLP database username.
const DB_USER_ENV: &str = "CLP_DB_USER";

/// The environment variable holding the CLP database password.
const DB_PASS_ENV: &str = "CLP_DB_PASS";

/// The synthetic S3 bucket name used for all generated object metadata.
const BENCHMARK_BUCKET: &str = "benchmark-bucket";

/// Database connection parameters shared by both benchmark binaries. Credentials are supplied
/// separately through the environment (see [`read_db_credentials`]).
#[derive(Clone, Debug, Deserialize)]
pub struct DatabaseConfig {
    /// The database host to connect to.
    pub host: String,

    /// The database port to connect to.
    pub port: u16,

    /// The CLP database (schema) name.
    pub name: String,
}

/// Buffer behavior for each ingestion job's compression buffer.
#[derive(Clone, Debug, Deserialize)]
pub struct BufferConfig {
    /// Size-based flush threshold, in bytes.
    pub flush_threshold_bytes: u64,

    /// Time-based flush threshold, in seconds.
    pub timeout_sec: u64,

    /// Capacity of the internal buffer channel.
    pub channel_capacity: usize,
}

/// The synthetic workload parameters that drive the mock ingestor.
#[derive(Clone, Debug, Deserialize)]
pub struct WorkloadConfig {
    /// The number of ingestion jobs to create.
    pub num_jobs: u32,

    /// The number of concurrent ingestion tasks per job.
    pub tasks_per_job: u32,

    /// The target ingestion rate, in object-metadata entries per minute, for each job. The rate is
    /// split evenly across the job's tasks. A value of `0` disables throttling.
    pub entries_per_minute_per_job: u32,

    /// The minimum number of objects per ingested batch (inclusive).
    pub batch_size_min: u32,

    /// The maximum number of objects per ingested batch (inclusive).
    pub batch_size_max: u32,

    /// The mean synthetic object size, in bytes. Each object's size is jittered by +/-25% around
    /// this value.
    pub object_size_bytes_mean: u64,

    /// The run duration, in seconds. A value of `0` runs until a termination signal is received.
    pub run_duration_sec: u64,
}

/// The full configuration for the mock ingestor binary.
#[derive(Clone, Debug, Deserialize)]
pub struct IngestorConfig {
    /// The database connection parameters.
    pub database: DatabaseConfig,

    /// The synthetic workload parameters.
    pub workload: WorkloadConfig,

    /// The per-job compression buffer parameters.
    pub buffer: BufferConfig,

    /// The interval, in seconds, between periodic database-call metric reports. A value of `0`
    /// disables periodic reports (the final summary is still printed).
    pub metrics_report_interval_sec: u64,
}

/// The full configuration for the mock scheduler binary.
#[derive(Clone, Debug, Deserialize)]
pub struct SchedulerConfig {
    /// The database connection parameters.
    pub database: DatabaseConfig,

    /// The interval, in milliseconds, between polls for newly submitted compression jobs.
    pub poll_interval_ms: u64,

    /// The maximum number of compression jobs processed concurrently.
    pub max_concurrent_jobs: usize,

    /// An artificial per-job compression delay, in milliseconds, applied before marking a job as
    /// succeeded. A value of `0` disables the delay.
    pub simulated_compression_duration_ms: u64,
}

/// A tiny deterministic `xorshift64` pseudo-random generator. It is seeded only from static
/// indices (never from wall-clock time) so that a benchmark run is fully reproducible.
pub struct Xorshift64 {
    /// The generator's internal state.
    state: u64,
}

impl Xorshift64 {
    /// # Returns
    ///
    /// A generator seeded from `seed`, forced non-zero so the sequence never collapses to zero.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self { state: seed | 1 }
    }

    /// Advances the generator.
    ///
    /// # Returns
    ///
    /// The next pseudo-random `u64`.
    pub const fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Draws a uniformly distributed value in the inclusive range `[low, high]`.
    ///
    /// # Returns
    ///
    /// A pseudo-random `u64` in `[low, high]`, or `low` when `high <= low`.
    pub const fn next_in_range(&mut self, low: u64, high: u64) -> u64 {
        if high <= low {
            return low;
        }
        let span = high - low + 1;
        low + self.next_u64() % span
    }
}

/// Initializes a `tracing` subscriber that writes to standard output.
///
/// Writing to standard output keeps logs visible through `docker compose logs`. The verbosity is
/// read from the `RUST_LOG` environment variable, defaulting to `info`.
///
/// # Panics
///
/// Panics if a global subscriber has already been installed for the current process.
pub fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stdout)
        .init();
}

/// Builds a [`ClpConfig`] for the given database parameters.
///
/// The config selects the S3 logs-input type with default AWS authentication, which is required by
/// [`ClpDbIngestionConnector::connect`] (it panics on the filesystem logs-input type). All other
/// fields fall back to their defaults.
///
/// # Returns
///
/// A [`ClpConfig`] on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`serde_json::from_value`]'s return values on failure.
pub fn build_clp_config(database: &DatabaseConfig) -> anyhow::Result<ClpConfig> {
    let config = serde_json::from_value(serde_json::json!({
        "database": {
            "host": database.host,
            "port": database.port,
            "names": { "clp": database.name, "spider": SPIDER_DB_NAME },
        },
        "logs_input": {
            "type": "s3",
            "aws_authentication": { "type": "default" },
        },
    }))
    .context("Failed to build CLP config from benchmark database parameters")?;
    Ok(config)
}

/// Builds a [`ClpDbConfig`] (the database section of [`ClpConfig`]) for use with
/// [`clp_rust_utils::database::mysql::create_clp_db_mysql_pool`].
///
/// # Returns
///
/// A [`ClpDbConfig`] populated from `database`.
#[must_use]
pub fn build_db_config(database: &DatabaseConfig) -> ClpDbConfig {
    ClpDbConfig {
        host: database.host.clone(),
        port: database.port,
        names: ClpDbNames {
            clp: database.name.clone(),
            spider: SPIDER_DB_NAME.to_owned(),
        },
    }
}

/// Reads the CLP database credentials from the environment.
///
/// # Returns
///
/// A [`ClpCredentials`] on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if the [`DB_USER_ENV`] environment variable is unset.
/// * [`anyhow::Error`] if the [`DB_PASS_ENV`] environment variable is unset.
pub fn read_db_credentials() -> anyhow::Result<ClpCredentials> {
    let user = std::env::var(DB_USER_ENV)
        .with_context(|| format!("Expect `{DB_USER_ENV}` env variable"))?;
    let password = std::env::var(DB_PASS_ENV)
        .with_context(|| format!("Expect `{DB_PASS_ENV}` env variable"))?;
    Ok(ClpCredentials {
        database: DbCredentials {
            password: SecretString::new(password.into_boxed_str()),
            user,
        },
    })
}

/// Builds an [`S3IngestionJobConfig`] for the SQS-listener path. The SQS-specific fields are set to
/// placeholder values because the benchmark drives ingestion directly and never contacts SQS.
///
/// # Returns
///
/// An [`S3IngestionJobConfig`] on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`serde_json::from_value`]'s return values on failure.
pub fn build_ingestion_job_config(
    job_index: u32,
    buffer: &BufferConfig,
) -> anyhow::Result<S3IngestionJobConfig> {
    let config = serde_json::from_value(serde_json::json!({
        "SqsListener": {
            "bucket_name": BENCHMARK_BUCKET,
            "key_prefix": format!("job{job_index}/"),
            "buffer_config": {
                "flush_threshold_bytes": buffer.flush_threshold_bytes,
                "timeout_sec": buffer.timeout_sec,
                "channel_capacity": buffer.channel_capacity,
            },
            "queue_url": "http://unused.invalid/queue",
            "num_concurrent_listener_tasks": 1,
            "wait_time_sec": 20,
        },
    }))
    .context("Failed to build S3 ingestion job config")?;
    Ok(config)
}

/// Connects to the CLP database, retrying indefinitely until the connection succeeds.
///
/// Retrying makes the benchmark resilient to container start ordering, where the database may not
/// be ready yet. The recovery context returned by [`ClpDbIngestionConnector::connect`] is discarded
/// because the benchmark always starts from freshly created jobs.
///
/// # Returns
///
/// A connected [`ClpDbIngestionConnector`].
pub async fn connect_with_retry(
    clp_config: &ClpConfig,
    credentials: &ClpCredentials,
    retry_interval: Duration,
) -> ClpDbIngestionConnector {
    loop {
        match ClpDbIngestionConnector::connect(clp_config.clone(), credentials.clone()).await {
            Ok((connector, _recovery)) => return connector,
            Err(error) => {
                tracing::warn!(
                    error = ? error,
                    retry_interval_sec = retry_interval.as_secs_f64(),
                    "Failed to connect to CLP DB; retrying."
                );
                tokio::time::sleep(retry_interval).await;
            }
        }
    }
}

/// Resolves once a `SIGTERM` or `Ctrl+C` signal is received.
///
/// # Panics
///
/// Panics if the `SIGTERM` signal handler cannot be installed.
pub async fn wait_for_termination_signal() {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("failed to install SIGTERM handler");
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

/// Generates a batch of synthetic [`ObjectMetadata`] with unique keys under the given job's key
/// prefix. Each object's size is jittered by +/-25% around `object_size_bytes_mean`.
///
/// # Returns
///
/// A vector of `batch_size` [`ObjectMetadata`] values. The `next_object_index` counter is advanced
/// by `batch_size` so subsequent calls produce non-overlapping keys.
///
/// # Panics
///
/// Panics if a generated key or bucket name is empty, which cannot happen because both are built
/// from non-empty format strings.
pub fn generate_batch(
    rng: &mut Xorshift64,
    job_index: u32,
    task_index: u32,
    next_object_index: &mut u64,
    batch_size: u32,
    object_size_bytes_mean: u64,
) -> Vec<ObjectMetadata> {
    let low = object_size_bytes_mean * 3 / 4;
    let high = object_size_bytes_mean + object_size_bytes_mean / 4;
    (0..batch_size)
        .map(|_| {
            let object_index = *next_object_index;
            *next_object_index += 1;
            ObjectMetadata {
                bucket: BENCHMARK_BUCKET
                    .parse()
                    .expect("benchmark bucket name is non-empty"),
                key: format!("job{job_index}/task{task_index}/obj{object_index}.log")
                    .parse()
                    .expect("object key is non-empty"),
                size: rng.next_in_range(low, high).max(1),
            }
        })
        .collect()
}

/// Formats a process-wide database-call metrics snapshot into a single human-readable line.
///
/// Averages are cumulative over the whole run: ingestion cost is reported per ingested entry (in
/// microseconds), while submission and completion costs are reported per call (in milliseconds).
/// Divide-by-zero cases (no recorded operations yet) are reported as `0`.
///
/// # Returns
///
/// A summary line such as
/// `ingestion_per_entry_us=42.1 submission_per_call_ms=3.80 completion_db_per_call_ms=1.20
/// (entries=33283 submits=14 completions=14)`.
#[must_use]
pub fn format_db_call_metrics(snapshot: &DbCallMetricsSnapshot) -> String {
    let ingestion_per_entry_us = if snapshot.ingestion_entries == 0 {
        0.0
    } else {
        snapshot.ingestion_total.as_secs_f64() * 1_000_000.0
            / count_as_f64(snapshot.ingestion_entries)
    };
    let submission_per_call_ms = if snapshot.submission_calls == 0 {
        0.0
    } else {
        snapshot.submission_total.as_secs_f64() * 1_000.0 / count_as_f64(snapshot.submission_calls)
    };
    let completion_db_per_call_ms = if snapshot.completion_calls == 0 {
        0.0
    } else {
        snapshot.completion_total.as_secs_f64() * 1_000.0 / count_as_f64(snapshot.completion_calls)
    };

    format!(
        "ingestion_per_entry_us={ingestion_per_entry_us:.1} \
         submission_per_call_ms={submission_per_call_ms:.2} \
         completion_db_per_call_ms={completion_db_per_call_ms:.2} \
         (entries={} submits={} completions={})",
        snapshot.ingestion_entries, snapshot.submission_calls, snapshot.completion_calls
    )
}

/// Converts a `u64` metric count to `f64` for averaging arithmetic.
///
/// # Returns
///
/// The value as an `f64`.
#[allow(
    clippy::cast_precision_loss,
    reason = "metric counts stay well below 2^53, so the conversion is effectively lossless"
)]
const fn count_as_f64(value: u64) -> f64 {
    value as f64
}
