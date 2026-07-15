//! Protocol types exchanged with the Spider (Huntsman) tasks that run CLP S3 compression jobs.
//!
//! Both the per-task input and output cross the Spider FFI boundary as an opaque, msgpack-encoded
//! `bytes` payload. See `claude/compression-coordinator-e2e-dev/spider-task-io.md` for the
//! end-to-end design.

use clp_rust_utils::clp_config::AwsAuthentication;
use non_empty_string::NonEmptyString;
use serde::{Deserialize, Serialize};

/// `clp-s` tuning and engine parameters for a compression job. Shared by every task in the job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClpSCompressionConfig {
    /// `--target-encoded-size`, i.e., `target_segment_size + target_dictionaries_size`.
    pub target_encoded_size: u64,

    /// `--compression-level`.
    pub compression_level: i32,

    /// `--timestamp-key`. `None` omits the flag.
    pub timestamp_key: Option<String>,
}

/// One compression task's partition: the exact S3 objects to compress.
///
/// One element of `submit_s3_compression_job`'s `inputs` vector.
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

/// Where a compression task writes its produced archives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct S3ArchiveOutputConfig {
    pub endpoint_url: Option<NonEmptyString>,

    pub region_code: Option<NonEmptyString>,

    pub bucket: NonEmptyString,

    pub key_prefix: NonEmptyString,

    pub aws_authentication: AwsAuthentication,
}

/// One compression task's complete input, assembled by `submit_s3_compression_job` from the
/// shared job config plus one [`S3InputSource`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressionTaskInput {
    /// `clp-s` tuning parameters (job-level; identical across a job's tasks).
    pub clp_s_config: ClpSCompressionConfig,

    /// Where to read the logs from (S3 read side).
    pub input: S3InputSource,

    /// Where to write the produced archives (S3 write side).
    pub output: S3ArchiveOutputConfig,

    /// Routes the local `clp-s` output subdir and the S3 output key, and is echoed into
    /// [`CompressionTaskOutput::dataset`] so the commit task can resolve the target
    /// `<prefix><dataset>_archives` table. `None` means no dataset.
    pub dataset: Option<String>,
}

/// Returned by one compression task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressionTaskOutput {
    /// The job's dataset (job-level; identical across every task, echoed from
    /// [`CompressionTaskInput::dataset`]). Lets the commit task resolve the target
    /// `<prefix><dataset>_archives` table from the output channel it already reads.
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
