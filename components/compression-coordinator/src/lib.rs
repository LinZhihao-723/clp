//! I/O protocol types and the compression-job-submission API trait for driving CLP S3 compression
//! jobs on a Spider (Huntsman) cluster.

pub mod compression_job_submitter;
mod error;
pub mod task;
pub mod task_io;

pub use compression_job_submitter::{CompressionJobCompletion, S3CompressionJobSubmitter};
pub use error::Error;
pub use task_io::{ArchiveMetadata, ClpSCompressionOption, CompressionTaskOutput, S3InputSource};
