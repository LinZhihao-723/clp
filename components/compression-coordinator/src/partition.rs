use std::path::PathBuf;

use clp_rust_utils::{
    job_config::{ClpIoConfig, CompressionJobId},
    s3::ObjectMetadata,
    serde::BrotliMsgpackBytes,
};
use sqlx::MySqlPool;

struct FileMetadata {
    path: PathBuf,
    size: u64,
    estimated_uncompressed_size: u64,
}

struct PathsToCompress {
    file_paths: Vec<String>,
    group_ids: Vec<usize>,
    st_sizes: Vec<u64>,
    empty_directories: Option<Vec<String>>,
}

struct PartitionInfo {
    partition_original_size: u64,
    clp_paths_to_compress: BrotliMsgpackBytes,
}

struct TaskArguments {
    job_id: CompressionJobId,
    task_id: i32,
    clp_io_config: ClpIoConfig,
    paths_to_compress: Option<PathsToCompress>,
    db_pool: MySqlPool,
}

pub(crate) struct PathsToCompressBuffer {
    files: Vec<FileMetadata>,
    tasks: Vec<TaskArguments>,
    partition_info: Vec<PartitionInfo>,
    maintain_file_ordering: bool,
    empty_directories: Option<Vec<String>>,
    total_file_size: u64,
    target_archive_size: u64,
    file_size_to_trigger_compression: u64,
    num_tasks: usize,
    task_arguments: TaskArguments,
}

impl PathsToCompressBuffer {
    pub(crate) fn new(
        scheduling_job_id: CompressionJobId,
        clp_io_config: ClpIoConfig,
        db_pool: MySqlPool,
    ) -> Self {
        let target_archive_size = clp_io_config.output.target_archive_size;

        Self {
            files: Vec::new(),
            tasks: Vec::new(),
            partition_info: Vec::new(),
            maintain_file_ordering: false,
            empty_directories: Some(Vec::new()),
            total_file_size: 0,
            target_archive_size,
            file_size_to_trigger_compression: target_archive_size * 2,
            num_tasks: 0,
            task_arguments: TaskArguments {
                job_id: scheduling_job_id,
                task_id: -1,
                clp_io_config,
                paths_to_compress: None,
                db_pool,
            },
        }
    }

    pub(crate) fn add_file(&mut self, metadata: ObjectMetadata) {
        self.total_file_size += metadata.estimated_uncompressed_size;
        self.files.push(FileMetadata {
            path: PathBuf::from(metadata.key.as_str()),
            size: metadata.size,
            estimated_uncompressed_size: metadata.estimated_uncompressed_size,
        });
    }
}
