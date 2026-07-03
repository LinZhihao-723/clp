//! OpenTelemetry metrics for log-ingestor.
//!
//! In addition to the OpenTelemetry counters, this module maintains a lightweight process-global
//! accumulator of database-call timings so that the average cost of the three main database
//! operations (metadata ingestion, compression-job submission, and compression-completion writes)
//! can be reported without an OpenTelemetry backend.

use std::{
    sync::{
        LazyLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use opentelemetry::metrics::Counter;

/// Records the ingestion of a chunk of S3 objects.
pub fn record_s3_ingestion(num_bytes: u64, num_objects: u64) {
    let metrics = &*METRICS;
    metrics.total_num_bytes.add(num_bytes, &[]);
    metrics.total_num_objects.add(num_objects, &[]);
}

/// Records the database cost (`duration`) of a successful ingestion write of `num_entries`
/// object-metadata entries.
pub fn record_ingestion_db(duration: Duration, num_entries: u64) {
    INGESTION_DB_NANOS.fetch_add(duration_as_nanos_u64(duration), Ordering::Relaxed);
    INGESTION_ENTRIES.fetch_add(num_entries, Ordering::Relaxed);
}

/// Records the database cost (`duration`) of a single `submit_for_compression` call.
pub fn record_compression_submission(duration: Duration) {
    SUBMISSION_NANOS.fetch_add(duration_as_nanos_u64(duration), Ordering::Relaxed);
    SUBMISSION_CALLS.fetch_add(1, Ordering::Relaxed);
}

/// Records the database cost (`duration`) of a single compression-completion write.
///
/// This excludes the time spent polling for the compression job to finish; only the final
/// metadata-update write is measured.
pub fn record_compression_completion_db(duration: Duration) {
    COMPLETION_DB_NANOS.fetch_add(duration_as_nanos_u64(duration), Ordering::Relaxed);
    COMPLETION_CALLS.fetch_add(1, Ordering::Relaxed);
}

/// Takes a snapshot of the process-global database-call accumulators.
///
/// # Returns
///
/// A [`DbCallMetricsSnapshot`] with the cumulative timings and counts observed so far.
#[must_use]
pub fn snapshot_db_call_metrics() -> DbCallMetricsSnapshot {
    DbCallMetricsSnapshot {
        ingestion_total: Duration::from_nanos(INGESTION_DB_NANOS.load(Ordering::Relaxed)),
        ingestion_entries: INGESTION_ENTRIES.load(Ordering::Relaxed),
        submission_total: Duration::from_nanos(SUBMISSION_NANOS.load(Ordering::Relaxed)),
        submission_calls: SUBMISSION_CALLS.load(Ordering::Relaxed),
        completion_total: Duration::from_nanos(COMPLETION_DB_NANOS.load(Ordering::Relaxed)),
        completion_calls: COMPLETION_CALLS.load(Ordering::Relaxed),
    }
}

/// Converts a [`Duration`] to nanoseconds as a `u64`, saturating at [`u64::MAX`] on overflow.
///
/// # Returns
///
/// The duration in nanoseconds, clamped to `u64`.
fn duration_as_nanos_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// Lazily-initialized global metrics instance.
static METRICS: LazyLock<LogIngestorMetrics> = LazyLock::new(|| {
    let meter = opentelemetry::global::meter("log-ingestor");
    LogIngestorMetrics {
        total_num_bytes: meter.u64_counter("clp.ingest.total_num_bytes").build(),
        total_num_objects: meter.u64_counter("clp.ingest.total_num_objects").build(),
    }
});

/// Telemetry metrics for tracking ingested S3 data.
struct LogIngestorMetrics {
    total_num_bytes: Counter<u64>,
    total_num_objects: Counter<u64>,
}

/// Cumulative nanoseconds spent on successful metadata ingestion database writes.
static INGESTION_DB_NANOS: AtomicU64 = AtomicU64::new(0);

/// Cumulative number of object-metadata entries written by successful ingestion database writes.
static INGESTION_ENTRIES: AtomicU64 = AtomicU64::new(0);

/// Cumulative nanoseconds spent on `submit_for_compression` database writes.
static SUBMISSION_NANOS: AtomicU64 = AtomicU64::new(0);

/// Cumulative number of `submit_for_compression` calls.
static SUBMISSION_CALLS: AtomicU64 = AtomicU64::new(0);

/// Cumulative nanoseconds spent on compression-completion database writes.
static COMPLETION_DB_NANOS: AtomicU64 = AtomicU64::new(0);

/// Cumulative number of compression-completion database writes.
static COMPLETION_CALLS: AtomicU64 = AtomicU64::new(0);

/// A point-in-time snapshot of the process-global database-call accumulators.
pub struct DbCallMetricsSnapshot {
    /// The cumulative time spent on successful metadata ingestion database writes.
    pub ingestion_total: Duration,

    /// The cumulative number of object-metadata entries ingested.
    pub ingestion_entries: u64,

    /// The cumulative time spent on `submit_for_compression` database writes.
    pub submission_total: Duration,

    /// The cumulative number of `submit_for_compression` calls.
    pub submission_calls: u64,

    /// The cumulative time spent on compression-completion database writes.
    pub completion_total: Duration,

    /// The cumulative number of compression-completion database writes.
    pub completion_calls: u64,
}
