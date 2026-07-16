//! The `compress` task: runs clp-s on an S3 partition and uploads the produced archives to S3.

use crate::task_io::{ClpSCompressionOption, CompressionTaskOutput, DbConfig, S3InputSource};

/// Compresses one partition of S3 objects into archives, uploads them to S3, and returns their
/// metadata for the commit task.
///
/// A pure worker function called by a spider-tdl task wrapper, which formats any returned
/// `anyhow::Error` into a user-space TDL error.
///
/// # Parameters
///
/// * `ctx` - The Spider task context (job/task identifiers).
/// * `clp_s_option` - clp-s tuning options for this job.
/// * `dataset` - The job's `CLP_S` dataset.
/// * `db_config` - Metadata-DB connection for the archive indexer.
/// * `input_source` - The S3 objects this task compresses.
///
/// # Returns
///
/// The metadata of every archive this task produced.
///
/// # Errors
///
/// Returns an error if any compression step fails (e.g., running clp-s or uploading to S3).
pub fn compress(
    ctx: &spider_tdl::TaskContext,
    clp_s_option: &ClpSCompressionOption,
    dataset: Option<String>,
    db_config: &DbConfig,
    input_source: S3InputSource,
) -> anyhow::Result<CompressionTaskOutput> {
    // Step 0: S3 URL generation.
    // Step 1: Archive generation.
    // Step 2: S3 upload.
    // Step 3: Call indexer.
    // Step 4: Delete local archives.
    let _ = (ctx, clp_s_option, dataset, db_config, input_source);
    todo!("implement compression steps")
}
