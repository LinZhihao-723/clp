use clp_rust_utils::{
    clp_config::S3Config,
    job_config::{ClpIoConfig, InputConfig},
    s3::ObjectMetadata,
};

use crate::{round_robin_partition::RoundRobinPartition, task_io::S3InputSource};

/// Buffers S3 object metadata and partitions the objects into Spider compression-task inputs.
pub(crate) struct PathsToCompressBuffer {
    /// Files waiting to be partitioned.
    files: Vec<ObjectMetadata>,

    /// Spider compression-task inputs created from completed partitions.
    tasks_input_sources: Vec<S3InputSource>,

    /// Whether files must retain their input ordering.
    maintain_file_ordering: bool,

    /// Estimated uncompressed size of all buffered files.
    total_file_size: u64,

    /// Desired uncompressed size of each archive.
    target_archive_size: u64,

    /// Buffered size at which partitioning is triggered.
    file_size_to_trigger_compression: u64,

    /// S3 settings shared by every generated input source.
    s3_config: S3Config,
}

impl PathsToCompressBuffer {
    /// Whether files must retain their input ordering while being partitioned.
    const MAINTAIN_FILE_ORDERING: bool = false;

    /// Creates an empty buffer using the compression target and S3 settings in `clp_io_config`.
    ///
    /// # Parameters
    ///
    /// * `clp_io_config` - The compression job's input and output configuration.
    ///
    /// # Returns
    ///
    /// A new empty [`PathsToCompressBuffer`].
    pub(crate) fn new(clp_io_config: &ClpIoConfig) -> Self {
        let target_archive_size = clp_io_config.output.target_archive_size;
        let s3_config = match &clp_io_config.input {
            InputConfig::S3InputConfig { config } => config.s3_config.clone(),
            InputConfig::S3ObjectMetadataInputConfig { config } => config.s3_config.clone(),
        };

        Self {
            files: Vec::new(),
            tasks_input_sources: Vec::new(),
            maintain_file_ordering: Self::MAINTAIN_FILE_ORDERING,
            total_file_size: 0,
            target_archive_size,
            file_size_to_trigger_compression: target_archive_size * 2,
            s3_config,
        }
    }

    /// Adds an S3 object to the buffer and partitions buffered objects when the trigger size is
    /// reached.
    ///
    /// # Parameters
    ///
    /// * `metadata` - Metadata for the S3 object to buffer.
    pub(crate) fn add_file(&mut self, metadata: ObjectMetadata) {
        self.total_file_size += metadata.estimated_uncompressed_size;
        self.files.push(metadata);
        if self.total_file_size >= self.file_size_to_trigger_compression {
            self.partition_and_compress(false);
        }
    }

    /// Gets the Spider compression-task inputs created from completed partitions.
    ///
    /// # Returns
    ///
    /// A slice containing one [`S3InputSource`] per completed partition.
    pub(crate) fn get_tasks_input_sources(&self) -> &[S3InputSource] {
        &self.tasks_input_sources
    }

    /// Partitions every buffered object, including a final partition smaller than the target size.
    pub(crate) fn flush(&mut self) {
        self.partition_and_compress(true);
    }

    /// Checks whether the buffer contains any objects waiting to be partitioned.
    ///
    /// # Returns
    ///
    /// `true` if at least one object remains buffered; `false` otherwise.
    fn contains_paths(&self) -> bool {
        !self.files.is_empty()
    }

    /// Converts a non-empty partition into a Spider compression-task input and stores it for later
    /// submission.
    ///
    /// # Parameters
    ///
    /// * `files` - The S3 objects belonging to the partition.
    fn submit_partition_for_compression(&mut self, files: Vec<ObjectMetadata>) {
        if files.is_empty() {
            return;
        }

        let object_keys = files
            .into_iter()
            .map(|metadata| String::from(metadata.key))
            .collect();

        self.tasks_input_sources.push(S3InputSource {
            endpoint_url: self.s3_config.endpoint_url.clone(),
            region_code: self.s3_config.region_code.clone(),
            bucket: self.s3_config.bucket.clone(),
            aws_authentication: self.s3_config.aws_authentication.clone(),
            object_keys,
        });
    }

    /// Partitions buffered objects into target-sized compression-task inputs.
    ///
    /// When `flush_buffer` is `false`, objects that cannot fill another target-sized partition
    /// remain buffered. When it is `true`, the final partial partition is also emitted.
    ///
    /// # Parameters
    ///
    /// * `flush_buffer` - Whether to emit a final partition smaller than the target size.
    fn partition_and_compress(&mut self, flush_buffer: bool) {
        if !flush_buffer && self.total_file_size < self.target_archive_size {
            // Not enough data for a full partition and we don't need to exhaust the buffer
            return;
        }
        if !self.contains_paths() {
            // Nothing to compress
            return;
        }

        let mut round_robin_partition = RoundRobinPartition::new(std::mem::take(&mut self.files));

        let mut has_more_files = true;
        while has_more_files && (flush_buffer || self.total_file_size >= self.target_archive_size) {
            let mut partition = Vec::new();
            let mut partition_size = 0;

            while partition_size < self.target_archive_size {
                let Some(file) = round_robin_partition.next() else {
                    has_more_files = false;
                    break;
                };

                partition_size += file.estimated_uncompressed_size;
                partition.push(file);
            }

            self.submit_partition_for_compression(partition);
            self.total_file_size -= partition_size;
        }

        // Keep any files that are insufficient to fill another target-sized archive buffered until
        // the next compression iteration.
        self.files = round_robin_partition.into_remaining_files();
    }
}
