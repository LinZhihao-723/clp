//! Protocol types exchanged with the Spider (Huntsman) tasks that run CLP S3 compression jobs.
//!
//! Each per-task input and the task output cross the Spider FFI boundary as an opaque,
//! msgpack-encoded `bytes` payload. See `claude/compression-coordinator-e2e-dev/spider-task-io.md`
//! for the end-to-end design.

use clp_rust_utils::clp_config::AwsAuthentication;
use non_empty_string::NonEmptyString;
use serde::{Deserialize, Serialize};

/// `clp-s` tuning and engine options for a compression job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClpSCompressionOption {
    /// `--target-encoded-size`, i.e., `target_segment_size + target_dictionaries_size`.
    pub target_encoded_size: u64,

    /// `--compression-level`.
    pub compression_level: i32,

    /// `--timestamp-key`. `None` omits the flag.
    pub timestamp_key: Option<String>,
}

/// One compression task's partition: the exact S3 objects to compress.
///
/// One element of `submit_s3_compression_job`'s `input_sources` vector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct S3InputSource {
    /// Custom/`MinIO` endpoint. `None` uses the default AWS endpoint.
    pub endpoint_url: Option<NonEmptyString>,

    pub region_code: Option<NonEmptyString>,

    pub bucket: NonEmptyString,

    pub aws_authentication: AwsAuthentication,

    /// S3 object keys to compress, already chosen by the partitioner. Each becomes one
    /// `--files-from` line.
    pub object_keys: Vec<String>,
}

/// Returned by one compression task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressionTaskOutput {
    /// The job's dataset (job-level; identical across every task). Lets the commit task resolve
    /// the target `<prefix><dataset>_archives` table from the output channel it already reads.
    pub dataset: Option<String>,

    /// One entry per archive this task produced (`clp-s` may emit several).
    pub archives: Vec<ArchiveMetadata>,
}

/// One produced archive.
///
/// The field set mirrors the values `update_archive_metadata` inserts into the CLP `archives`
/// table (`components/job-orchestration/job_orchestration/executor/compress/compression_task.py`,
/// lines 113-144), minus the `creator_id`/`creation_ix` defaults the commit task injects at
/// insert time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveMetadata {
    /// `archives.id`.
    pub id: String,

    /// `archives.begin_timestamp`.
    pub begin_timestamp: i64,

    /// `archives.end_timestamp`.
    pub end_timestamp: i64,

    /// `archives.size` (compressed).
    pub size: i64,

    /// `archives.uncompressed_size`.
    pub uncompressed_size: i64,
}
