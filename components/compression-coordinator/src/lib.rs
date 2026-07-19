//! I/O protocol types and the compression-job-submission API trait for driving CLP S3 compression
//! jobs on a Spider (Huntsman) cluster.

pub mod compression_job_submitter;
mod coordinator;
mod error;
mod partition;
mod round_robin_partition;
mod spider_submitter;
pub mod task;
pub mod task_io;

pub use compression_job_submitter::{CompressionJobCompletion, S3CompressionJobSubmitter};
pub use coordinator::CompressionCoordinator;
pub use error::Error;
pub use task::{commit::commit, s3_compression::compress, spider_task_executor_config};
pub use task_io::{ArchiveMetadata, ClpSCompressionOption, CompressionTaskOutput, S3InputSource};
