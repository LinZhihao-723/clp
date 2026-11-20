//! The clp-s JSON extraction worker that extracts one archive into ordered JSON stream chunks, and
//! publishes them to the stream output and the results cache.

use std::ffi::OsString;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::num::NonZeroU64;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStderr;
use std::process::ChildStdout;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::thread::ScopedJoinHandle;

use anyhow::Context;
use clp_rust_utils::clp_config::package::config::ResultsCache;
use clp_rust_utils::clp_config::package::config::SpiderTaskExecutorConfig;
use clp_rust_utils::clp_config::package::config::StorageEngine;
use clp_rust_utils::clp_config::package::config::StreamOutputStorage;
use clp_rust_utils::task_io::extraction::ExtractTaskInput;
use clp_rust_utils::types::ArchiveId;
use mongodb::Client;
use mongodb::bson::Document;
use mongodb::bson::doc;
use mongodb::options::ReplaceOneModel;
use serde::Deserialize;
use spider_tdl::TaskContext;

use crate::common::clp_home;
use crate::common::runtime;
use crate::task::query::archive_input::ArchiveDescriptor;
use crate::task::query::archive_input::resolve_archive_input;
use crate::task::query::results_cache::parse_results_cache_uri;
use crate::task::utils::build_s3_client;
use crate::task::utils::clp_binary_path;
use crate::task::utils::upload_file_to_s3;

/// Extracts one archive into ordered JSON stream chunks with clp-s, then publishes the chunk files
/// to the stream output and their metadata to the results cache.
///
/// Every chunk file is published before any metadata document, and both are keyed by the chunk's
/// file name, which is determined by its archive and range. So every visible metadata document
/// points at a published file, and the task is safe to retry.
///
/// A pure worker function called by a spider-tdl task wrapper, which formats any returned
/// `anyhow::Error` into a user-space TDL error.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if the configured storage engine is not [`StorageEngine::ClpS`].
/// * Forwards [`resolve_archive_input`]'s return values on failure.
/// * Forwards [`StagingDir::create`]'s return values on failure.
/// * Forwards [`run_clp_s_extract`]'s return values on failure.
/// * Forwards [`StreamChunk::parse_all`]'s return values on failure.
/// * Forwards [`publish_chunk_files`]'s return values on failure.
/// * Forwards [`publish_chunk_metadata`]'s return values on failure.
/// * Forwards [`StagingDir::remove`]'s return values on failure.
pub(super) fn extract_json(
    ctx: &TaskContext,
    config: &SpiderTaskExecutorConfig,
    input: &ExtractTaskInput,
) -> anyhow::Result<()> {
    const STAGING_DIRECTORY_NAME: &str = ".staging";

    anyhow::ensure!(
        StorageEngine::ClpS == config.package.storage_engine,
        "the clp-s JSON extraction task requires the `clp-s` storage engine"
    );

    tracing::info!(
        job_id = % ctx.job_id,
        task_id = % ctx.task_id,
        task_instance_id = % ctx.task_instance_id,
        query_job_id = % input.job_id,
        dataset = % input.dataset,
        archive_id = % input.archive_id,
        "clp-s JSON extraction task started."
    );

    let clp_home = clp_home();
    let runtime = runtime();
    let (archive_descriptor, credential_env) =
        resolve_archive_input(&runtime, clp_home, config, &input.dataset, input.archive_id)?;
    let staging_dir = StagingDir::create(
        config
            .abs_stream_output_staging(clp_home)
            .join(STAGING_DIRECTORY_NAME)
            .join(format!("{}.{}", input.job_id, ctx.task_instance_id)),
    )?;
    let chunk_paths = run_clp_s_extract(
        clp_home,
        &archive_descriptor,
        &credential_env,
        staging_dir.path(),
        input.target_chunk_size,
    )?;
    let chunks = StreamChunk::parse_all(input.archive_id, &chunk_paths)?;
    publish_chunk_files(&runtime, clp_home, config, staging_dir.path(), &chunks)?;
    runtime.block_on(publish_chunk_metadata(
        &config.results_cache,
        input.archive_id,
        &chunks,
    ))?;
    staging_dir.remove()?;

    tracing::info!(
        job_id = % ctx.job_id,
        task_id = % ctx.task_id,
        task_instance_id = % ctx.task_instance_id,
        query_job_id = % input.job_id,
        num_chunks = chunks.len(),
        "clp-s JSON extraction task completed successfully."
    );
    Ok(())
}

/// The maximum size of one line of the chunk statistics clp-s prints, excluding its newline.
const MAX_STATS_LINE_SIZE: usize = 64 * 1024;

/// One ordered JSON stream chunk that clp-s extracted, in a file named
/// `{archive_id}_{begin_msg_ix}_{end_msg_ix}.jsonl`.
#[derive(Debug, Eq, PartialEq)]
struct StreamChunk {
    file_name: String,
    begin_msg_ix: i64,
    end_msg_ix: i64,
    is_last_chunk: bool,
}

impl StreamChunk {
    /// Parses the chunk files clp-s reported for the archive with ID `archive_id`, in the order it
    /// reported them.
    ///
    /// # Returns
    ///
    /// The chunks on success, in the reported order, where only the last one is marked as the last
    /// chunk. No paths give no chunks.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`StreamChunkError::InvalidPath`] if a path has no UTF-8 file name.
    /// * [`StreamChunkError::MalformedFileName`] if a file name isn't
    ///   `{archive_id}_{begin_msg_ix}_{end_msg_ix}.jsonl` with `i64` bounds.
    /// * [`StreamChunkError::EmptyRange`] if a chunk doesn't end after it begins.
    /// * [`StreamChunkError::NotContiguous`] if a chunk doesn't begin where the previous one ends.
    fn parse_all(archive_id: ArchiveId, paths: &[PathBuf]) -> Result<Vec<Self>, StreamChunkError> {
        let file_name_prefix = format!("{archive_id}_");
        let mut chunks: Vec<Self> = Vec::with_capacity(paths.len());
        for path in paths {
            let file_name = path
                .file_name()
                .and_then(|file_name| file_name.to_str())
                .ok_or_else(|| StreamChunkError::InvalidPath { path: path.clone() })?;
            let Some((begin_msg_ix, end_msg_ix)) = file_name
                .strip_prefix(file_name_prefix.as_str())
                .and_then(|file_name| file_name.strip_suffix(".jsonl"))
                .and_then(|range| range.split_once('_'))
                .and_then(|(begin, end)| {
                    Some((begin.parse::<i64>().ok()?, end.parse::<i64>().ok()?))
                })
            else {
                return Err(StreamChunkError::MalformedFileName {
                    file_name: file_name.to_owned(),
                });
            };
            if begin_msg_ix >= end_msg_ix {
                return Err(StreamChunkError::EmptyRange {
                    file_name: file_name.to_owned(),
                });
            }
            if let Some(previous) = chunks.last_mut() {
                if previous.end_msg_ix != begin_msg_ix {
                    return Err(StreamChunkError::NotContiguous {
                        file_name: file_name.to_owned(),
                        begin_msg_ix,
                        previous_end_msg_ix: previous.end_msg_ix,
                    });
                }
                previous.is_last_chunk = false;
            }
            chunks.push(Self {
                file_name: file_name.to_owned(),
                begin_msg_ix,
                end_msg_ix,
                is_last_chunk: true,
            });
        }
        Ok(chunks)
    }

    /// # Returns
    ///
    /// The chunk's results-cache metadata document, which is identified by, and points at, the
    /// chunk's file name.
    fn metadata_document(&self, archive_id: ArchiveId) -> Document {
        doc! {
            "_id": self.file_name.as_str(),
            "path": self.file_name.as_str(),
            "stream_id": archive_id.to_string(),
            "begin_msg_ix": self.begin_msg_ix,
            "end_msg_ix": self.end_msg_ix,
            "is_last_chunk": self.is_last_chunk,
        }
    }
}

/// Errors from [`StreamChunk::parse_all`].
#[derive(Debug, Eq, PartialEq, thiserror::Error)]
enum StreamChunkError {
    #[error("clp-s reported a chunk path without a UTF-8 file name: {path:?}")]
    InvalidPath { path: PathBuf },

    #[error("chunk file name `{file_name}` isn't `<archive-id>_<begin>_<end>.jsonl`")]
    MalformedFileName { file_name: String },

    #[error("chunk `{file_name}` doesn't end after it begins")]
    EmptyRange { file_name: String },

    #[error(
        "chunk `{file_name}` begins at {begin_msg_ix}, not where the previous chunk ends \
         ({previous_end_msg_ix})"
    )]
    NotContiguous {
        file_name: String,
        begin_msg_ix: i64,
        previous_end_msg_ix: i64,
    },
}

/// One line of the chunk statistics clp-s prints with `--print-ordered-chunk-stats`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChunkStatsLine {
    path: PathBuf,
}

/// Errors from [`read_chunk_paths`].
#[derive(Debug, thiserror::Error)]
enum ChunkStatsError {
    #[error("a line of clp-s's chunk statistics is longer than {MAX_STATS_LINE_SIZE} B")]
    LineTooLong,

    #[error("the last line of clp-s's chunk statistics isn't newline-terminated")]
    UnterminatedLine,

    #[error("clp-s reported more than {max_num_chunks} chunks")]
    TooManyChunks { max_num_chunks: usize },

    #[error("failed to read clp-s's chunk statistics: {0}")]
    Read(#[from] std::io::Error),

    #[error("malformed line in clp-s's chunk statistics: {0}")]
    MalformedLine(#[from] serde_json::Error),
}

/// A directory, unique to one task instance, that clp-s extracts chunk files into. It's removed
/// with its contents when dropped.
#[must_use]
struct StagingDir {
    path: Option<PathBuf>,
}

impl StagingDir {
    /// Factory function.
    ///
    /// Creates the directory at `path`, along with its missing parents.
    ///
    /// # Returns
    ///
    /// A newly created [`StagingDir`] on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`std::fs::create_dir_all`]'s return values on failure.
    /// * Forwards [`std::fs::create_dir`]'s return values on failure, including if the directory
    ///   already exists.
    fn create(path: PathBuf) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create the directory {}", parent.display()))?;
        }
        std::fs::create_dir(&path).with_context(|| {
            format!("failed to create the staging directory {}", path.display())
        })?;
        Ok(Self { path: Some(path) })
    }

    /// # Returns
    ///
    /// The staging directory's path.
    ///
    /// # Panics
    ///
    /// Panics if the path was taken, which only the consuming [`Self::remove`] does.
    fn path(&self) -> &Path {
        self.path
            .as_deref()
            .expect("only `remove` takes the path, and it consumes the staging directory")
    }

    /// Removes the staging directory with its contents.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`std::fs::remove_dir_all`]'s return values on failure.
    ///
    /// # Panics
    ///
    /// Panics if the path was already taken, which only this consuming method does.
    fn remove(mut self) -> anyhow::Result<()> {
        let path = self
            .path
            .take()
            .expect("only `remove` takes the path, and it consumes the staging directory");
        std::fs::remove_dir_all(&path)
            .with_context(|| format!("failed to remove the staging directory {}", path.display()))
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {}
            Err(e) if std::io::ErrorKind::NotFound == e.kind() => {}
            Err(e) => {
                tracing::warn!(
                    error = % e,
                    path = % path.display(),
                    "Failed to remove the JSON extraction staging directory."
                );
            }
        }
    }
}

/// A spawned child process that's killed and reaped when dropped, unless a consuming method
/// already reaped it.
#[must_use]
struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    /// Waits for the child to exit, and reaps it.
    ///
    /// # Returns
    ///
    /// The child's exit status on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Child::wait`]'s return values on failure, after which the child is killed and
    ///   reaped on drop.
    ///
    /// # Panics
    ///
    /// Panics if the child was already taken, which only consuming methods do.
    fn wait(mut self) -> std::io::Result<ExitStatus> {
        let status = self
            .child
            .as_mut()
            .expect("only consuming methods take the child")
            .wait()?;
        self.child = None;
        Ok(status)
    }

    /// Kills the child, and reaps it.
    ///
    /// # Returns
    ///
    /// The killed child's exit status on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Child::kill`]'s return values on failure.
    /// * Forwards [`Child::wait`]'s return values on failure.
    ///
    /// # Panics
    ///
    /// Panics if the child was already taken, which only consuming methods do.
    fn kill_and_reap(mut self) -> std::io::Result<ExitStatus> {
        let child = self
            .child
            .as_mut()
            .expect("only consuming methods take the child");
        child.kill()?;
        let status = child.wait()?;
        self.child = None;
        Ok(status)
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if let Err(e) = child.kill() {
            tracing::warn!(error = % e, pid = child.id(), "Failed to kill the child process.");
        }
        if let Err(e) = child.wait() {
            tracing::warn!(error = % e, pid = child.id(), "Failed to reap the child process.");
        }
    }
}

/// A [`ChildGuard`] with the child's piped stdout and stderr.
struct PipedChild {
    guard: ChildGuard,
    stdout: ChildStdout,
    stderr: ChildStderr,
}

impl PipedChild {
    /// Factory function.
    ///
    /// Spawns `command` with a null stdin and piped stdout and stderr.
    ///
    /// # Returns
    ///
    /// The guarded child with its stdout and stderr on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Command::spawn`]'s return values on failure.
    ///
    /// # Panics
    ///
    /// Panics if the spawned child's piped stdout or stderr is absent, which [`Command::spawn`]
    /// rules out.
    fn spawn(mut command: Command) -> std::io::Result<Self> {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let guard = ChildGuard { child: Some(child) };
        Ok(Self {
            guard,
            stdout: stdout.expect("a piped stdout should always be present"),
            stderr: stderr.expect("a piped stderr should always be present"),
        })
    }
}

/// Runs `clp-s x` to extract the archive in log order into chunk files in `output_directory`.
///
/// clp-s has been reaped by the time this function returns, on every path.
///
/// # Returns
///
/// The paths of the chunk files clp-s reported on success, in log order.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if clp-s exits with a non-zero status, with the tail of its stderr.
/// * Forwards [`PipedChild::spawn`]'s return values on failure.
/// * Forwards [`read_chunk_paths`]'s return values on failure, with the tail of clp-s's stderr.
/// * Forwards [`ChildGuard::wait`]'s return values on failure.
/// * Forwards [`ChildGuard::kill_and_reap`]'s return values on failure.
fn run_clp_s_extract(
    clp_home: &Path,
    archive_descriptor: &ArchiveDescriptor,
    credential_env: &[(&'static str, String)],
    output_directory: &Path,
    target_chunk_size: Option<NonZeroU64>,
) -> anyhow::Result<Vec<PathBuf>> {
    const MAX_STDERR_TAIL_SIZE: usize = 64 * 1024;

    let clp_s_bin = clp_binary_path(clp_home, "clp-s");
    let mut command = Command::new(&clp_s_bin);
    command
        .args(build_clp_s_extract_args(
            archive_descriptor,
            output_directory,
            target_chunk_size,
        ))
        .envs(credential_env.iter().cloned());
    let PipedChild {
        guard,
        stdout,
        stderr,
    } = PipedChild::spawn(command)
        .with_context(|| format!("failed to spawn clp-s at {}", clp_s_bin.display()))?;

    std::thread::scope(move |scope| {
        // Owned by this frame, so that a panic below kills and reaps clp-s, closing its stderr,
        // before the scope joins the draining thread.
        let guard = guard;
        let stderr_tail = scope.spawn(move || drain_tail(stderr, MAX_STDERR_TAIL_SIZE));
        let chunk_paths = read_chunk_paths(BufReader::new(stdout));
        let status = match &chunk_paths {
            Ok(_) => guard.wait(),
            Err(_) => guard.kill_and_reap(),
        };
        let stderr_tail = describe_stderr_tail(stderr_tail);
        let chunk_paths = chunk_paths.with_context(|| {
            format!(
                "failed to read clp-s's chunk statistics; clp-s's stderr ends with: {stderr_tail}"
            )
        })?;
        let status = status.context("failed to wait for clp-s to exit")?;
        anyhow::ensure!(
            status.success(),
            "clp-s exited with {status}; its stderr ends with: {stderr_tail}"
        );
        Ok(chunk_paths)
    })
}

/// Builds the arguments of the `clp-s x` command that extracts the archive in log order into chunk
/// files in `output_directory`, printing each chunk's statistics.
///
/// # Returns
///
/// The ordered clp-s arguments, starting with the `x` command.
fn build_clp_s_extract_args(
    archive_descriptor: &ArchiveDescriptor,
    output_directory: &Path,
    target_chunk_size: Option<NonZeroU64>,
) -> Vec<OsString> {
    let mut args = vec![OsString::from("x")];
    args.extend(archive_descriptor.clp_s_args());
    args.push(output_directory.as_os_str().to_os_string());
    args.push(OsString::from("--ordered"));
    if let Some(target_chunk_size) = target_chunk_size {
        args.push(OsString::from("--target-ordered-chunk-size"));
        args.push(OsString::from(target_chunk_size.to_string()));
    }
    args.push(OsString::from("--print-ordered-chunk-stats"));
    args
}

/// Reads the chunk statistics clp-s prints with `--print-ordered-chunk-stats`, one JSON object
/// per line, stopping at the first line that can't be parsed.
///
/// # Type Parameters
///
/// * `ReaderType` - The type of the reader over clp-s's stdout.
///
/// # Returns
///
/// The path of every reported chunk file on success, in the reported order.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`ChunkStatsError::LineTooLong`] if a line is longer than [`MAX_STATS_LINE_SIZE`].
/// * [`ChunkStatsError::UnterminatedLine`] if the output ends in the middle of a line.
/// * [`ChunkStatsError::TooManyChunks`] if more than `MAX_NUM_CHUNKS` lines are printed.
/// * [`ChunkStatsError::Read`] if [`BufRead::read_until`] fails.
/// * [`ChunkStatsError::MalformedLine`] if [`serde_json::from_slice`] fails to parse a line.
///
/// # Panics
///
/// Panics if `MAX_STATS_LINE_SIZE + 1` doesn't fit in a `u64`.
fn read_chunk_paths<ReaderType: BufRead>(
    mut reader: ReaderType,
) -> Result<Vec<PathBuf>, ChunkStatsError> {
    const MAX_NUM_CHUNKS: usize = 1_000_000;
    let max_read_size = u64::try_from(MAX_STATS_LINE_SIZE + 1)
        .expect("the maximum stats line size should fit in a `u64`");

    let mut paths = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        if 0 == (&mut reader)
            .take(max_read_size)
            .read_until(b'\n', &mut line)?
        {
            return Ok(paths);
        }
        let Some(content) = line.strip_suffix(b"\n") else {
            return Err(if line.len() > MAX_STATS_LINE_SIZE {
                ChunkStatsError::LineTooLong
            } else {
                ChunkStatsError::UnterminatedLine
            });
        };
        if paths.len() == MAX_NUM_CHUNKS {
            return Err(ChunkStatsError::TooManyChunks {
                max_num_chunks: MAX_NUM_CHUNKS,
            });
        }
        paths.push(serde_json::from_slice::<ChunkStatsLine>(content)?.path);
    }
}

/// Reads `reader` to its end, keeping only the last `max_size` bytes.
///
/// # Type Parameters
///
/// * `ReaderType` - The type of the reader to drain.
///
/// # Returns
///
/// The last `max_size` bytes read on success, or every byte read if there are fewer.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`Read::read_to_end`]'s return values on failure.
fn drain_tail<ReaderType: Read>(
    mut reader: ReaderType,
    max_size: usize,
) -> std::io::Result<Vec<u8>> {
    const READ_SIZE: u64 = 8 * 1024;

    let mut tail = Vec::new();
    while 0 != (&mut reader).take(READ_SIZE).read_to_end(&mut tail)? {
        if tail.len() > max_size {
            tail.drain(..tail.len() - max_size);
        }
    }
    Ok(tail)
}

/// Joins the thread draining clp-s's stderr, and describes the tail it read.
///
/// # Returns
///
/// The tail decoded lossily as UTF-8, or a description of why it couldn't be read.
fn describe_stderr_tail(stderr_tail: ScopedJoinHandle<'_, std::io::Result<Vec<u8>>>) -> String {
    match stderr_tail.join() {
        Ok(Ok(tail)) => String::from_utf8_lossy(&tail).into_owned(),
        Ok(Err(e)) => format!("<failed to read clp-s's stderr: {e}>"),
        Err(_) => "<the thread draining clp-s's stderr panicked>".to_owned(),
    }
}

/// Publishes the chunk files from `staging_directory` to the stream output.
///
/// A file that already exists at the destination is overwritten, since a chunk's content is
/// determined by its file name.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if a chunk file is larger than the 5 GiB that S3's `PutObject` accepts.
/// * Forwards [`std::fs::rename`]'s return values on failure.
/// * Forwards [`std::fs::metadata`]'s return values on failure.
/// * Forwards [`upload_file_to_s3`]'s return values on failure.
fn publish_chunk_files(
    runtime: &tokio::runtime::Handle,
    clp_home: &Path,
    config: &SpiderTaskExecutorConfig,
    staging_directory: &Path,
    chunks: &[StreamChunk],
) -> anyhow::Result<()> {
    const MAX_PUT_OBJECT_SIZE: u64 = 5 * 1024 * 1024 * 1024;

    match &config.stream_output.storage {
        StreamOutputStorage::Fs { .. } => {
            let stream_directory = config.abs_stream_output_staging(clp_home);
            for chunk in chunks {
                let source = staging_directory.join(&chunk.file_name);
                let destination = stream_directory.join(&chunk.file_name);
                std::fs::rename(&source, &destination).with_context(|| {
                    format!(
                        "failed to move {} to {}",
                        source.display(),
                        destination.display()
                    )
                })?;
            }
        }
        StreamOutputStorage::S3 { s3_config, .. } => {
            let client = build_s3_client(runtime, s3_config);
            for chunk in chunks {
                let path = staging_directory.join(&chunk.file_name);
                let size = std::fs::metadata(&path)
                    .with_context(|| format!("failed to read the metadata of {}", path.display()))?
                    .len();
                anyhow::ensure!(
                    size <= MAX_PUT_OBJECT_SIZE,
                    "chunk file {} is {size} B, more than the 5 GiB that S3's `PutObject` accepts",
                    path.display()
                );
                runtime.block_on(upload_file_to_s3(
                    &client,
                    s3_config.bucket.as_str(),
                    &format!("{}{}", s3_config.key_prefix, chunk.file_name),
                    &path,
                ))?;
            }
        }
    }
    Ok(())
}

/// Upserts each chunk's metadata document into the results cache's stream collection, in bounded
/// batches.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`parse_results_cache_uri`]'s return values on failure.
/// * Forwards [`Client::with_options`]'s return values on failure.
/// * Forwards [`Client::bulk_write`]'s return values on failure.
async fn publish_chunk_metadata(
    results_cache: &ResultsCache,
    archive_id: ArchiveId,
    chunks: &[StreamChunk],
) -> anyhow::Result<()> {
    const BATCH_SIZE: usize = 1_000;

    if chunks.is_empty() {
        return Ok(());
    }
    let mut options = parse_results_cache_uri(results_cache.uri().as_str()).await?;
    options.app_name = Some("clp-json-extraction".to_owned());
    let client = Client::with_options(options)?;
    let collection = client
        .database(&results_cache.db_name)
        .collection::<Document>(&results_cache.stream_collection_name);
    for batch in chunks.chunks(BATCH_SIZE) {
        let models: Vec<_> = batch
            .iter()
            .map(|chunk| {
                ReplaceOneModel::builder()
                    .namespace(collection.namespace())
                    .filter(doc! { "_id": chunk.file_name.as_str() })
                    .replacement(chunk.metadata_document(archive_id))
                    .upsert(true)
                    .build()
            })
            .collect();
        client
            .bulk_write(models)
            .await
            .context("failed to publish the JSON stream chunks' metadata")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::io::Write;
    use std::num::NonZeroU64;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::path::PathBuf;
    use std::time::Duration;

    use clp_rust_utils::types::ArchiveId;
    use mongodb::bson::doc;

    use super::ChunkStatsError;
    use super::MAX_STATS_LINE_SIZE;
    use super::StreamChunk;
    use super::StreamChunkError;
    use super::build_clp_s_extract_args;
    use super::drain_tail;
    use super::read_chunk_paths;
    use super::run_clp_s_extract;
    use crate::task::query::archive_input::ArchiveDescriptor;

    const ARCHIVE_ID: &str = "018e90e5-8b2a-4a61-a2fc-cac799936caf";

    /// # Returns
    ///
    /// [`ARCHIVE_ID`] as an [`ArchiveId`].
    ///
    /// # Panics
    ///
    /// Panics if [`ARCHIVE_ID`] isn't a valid UUID.
    fn archive_id() -> ArchiveId {
        ARCHIVE_ID.parse::<ArchiveId>().expect("valid archive UUID")
    }

    /// # Returns
    ///
    /// The path clp-s reports for a chunk file named `file_name` in its output directory.
    fn chunk_path(file_name: &str) -> PathBuf {
        Path::new("/staging").join(file_name)
    }

    #[test]
    fn parse_all_rejects_malformed_chunk_files() {
        let cases = [
            (
                vec![chunk_path("11111111-2222-4333-8444-555555555555_0_5.jsonl")],
                StreamChunkError::MalformedFileName {
                    file_name: "11111111-2222-4333-8444-555555555555_0_5.jsonl".to_owned(),
                },
            ),
            (
                vec![chunk_path(&format!("{ARCHIVE_ID}_a_5.jsonl"))],
                StreamChunkError::MalformedFileName {
                    file_name: format!("{ARCHIVE_ID}_a_5.jsonl"),
                },
            ),
            (
                vec![chunk_path(&format!("{ARCHIVE_ID}_0_5_9.jsonl"))],
                StreamChunkError::MalformedFileName {
                    file_name: format!("{ARCHIVE_ID}_0_5_9.jsonl"),
                },
            ),
            (
                vec![chunk_path(&format!("{ARCHIVE_ID}_0_5.json"))],
                StreamChunkError::MalformedFileName {
                    file_name: format!("{ARCHIVE_ID}_0_5.json"),
                },
            ),
            (
                vec![PathBuf::from("/")],
                StreamChunkError::InvalidPath {
                    path: PathBuf::from("/"),
                },
            ),
            (
                vec![chunk_path(&format!("{ARCHIVE_ID}_5_5.jsonl"))],
                StreamChunkError::EmptyRange {
                    file_name: format!("{ARCHIVE_ID}_5_5.jsonl"),
                },
            ),
            (
                vec![chunk_path(&format!("{ARCHIVE_ID}_6_5.jsonl"))],
                StreamChunkError::EmptyRange {
                    file_name: format!("{ARCHIVE_ID}_6_5.jsonl"),
                },
            ),
            (
                vec![
                    chunk_path(&format!("{ARCHIVE_ID}_0_5.jsonl")),
                    chunk_path(&format!("{ARCHIVE_ID}_6_9.jsonl")),
                ],
                StreamChunkError::NotContiguous {
                    file_name: format!("{ARCHIVE_ID}_6_9.jsonl"),
                    begin_msg_ix: 6,
                    previous_end_msg_ix: 5,
                },
            ),
            (
                vec![
                    chunk_path(&format!("{ARCHIVE_ID}_0_5.jsonl")),
                    chunk_path(&format!("{ARCHIVE_ID}_4_9.jsonl")),
                ],
                StreamChunkError::NotContiguous {
                    file_name: format!("{ARCHIVE_ID}_4_9.jsonl"),
                    begin_msg_ix: 4,
                    previous_end_msg_ix: 5,
                },
            ),
        ];

        for (paths, expected) in cases {
            assert_eq!(
                StreamChunk::parse_all(archive_id(), &paths),
                Err(expected),
                "paths: {paths:?}"
            );
        }
    }

    #[test]
    fn parse_all_accepts_zero_chunks() {
        assert_eq!(StreamChunk::parse_all(archive_id(), &[]), Ok(Vec::new()));
    }

    #[test]
    fn parse_all_outputs_contiguous_chunks_with_only_the_last_marked() -> anyhow::Result<()> {
        for first_begin_msg_ix in [0_i64, 3, 1_000_000_007] {
            for num_chunks in 1..=8_i64 {
                let mut bounds = vec![first_begin_msg_ix];
                for index in 0..num_chunks {
                    let previous = *bounds.last().expect("bounds start non-empty");
                    bounds.push(previous + 1 + (index * 7) % 5);
                }
                let file_names: Vec<String> = bounds
                    .windows(2)
                    .map(|range| format!("{ARCHIVE_ID}_{}_{}.jsonl", range[0], range[1]))
                    .collect();
                let paths: Vec<PathBuf> = file_names.iter().map(|name| chunk_path(name)).collect();

                let chunks = StreamChunk::parse_all(archive_id(), &paths)?;

                assert_eq!(
                    chunks
                        .iter()
                        .map(|chunk| chunk.file_name.as_str())
                        .collect::<Vec<_>>(),
                    file_names
                );
                assert_eq!(
                    chunks.first().map(|chunk| chunk.begin_msg_ix),
                    Some(first_begin_msg_ix)
                );
                assert!(
                    chunks
                        .iter()
                        .all(|chunk| chunk.begin_msg_ix < chunk.end_msg_ix)
                );
                assert!(
                    chunks
                        .windows(2)
                        .all(|pair| pair[0].end_msg_ix == pair[1].begin_msg_ix)
                );
                assert_eq!(
                    chunks
                        .iter()
                        .map(|chunk| chunk.is_last_chunk)
                        .collect::<Vec<_>>(),
                    (0..num_chunks)
                        .map(|index| index == num_chunks - 1)
                        .collect::<Vec<_>>()
                );
            }
        }
        Ok(())
    }

    #[test]
    fn metadata_document_is_keyed_by_and_points_at_the_chunk_file() -> anyhow::Result<()> {
        let file_name = format!("{ARCHIVE_ID}_10_20.jsonl");
        let chunks = StreamChunk::parse_all(archive_id(), &[chunk_path(&file_name)])?;
        let [chunk] = chunks.as_slice() else {
            panic!("expected exactly one chunk, got {chunks:?}");
        };

        assert_eq!(
            chunk.metadata_document(archive_id()),
            doc! {
                "_id": file_name.as_str(),
                "path": file_name.as_str(),
                "stream_id": ARCHIVE_ID,
                "begin_msg_ix": 10_i64,
                "end_msg_ix": 20_i64,
                "is_last_chunk": true,
            }
        );
        Ok(())
    }

    #[test]
    fn build_clp_s_extract_args_passes_the_chunk_size_only_when_targeted() {
        let archive_descriptor = ArchiveDescriptor::Directory {
            path: PathBuf::from("/archives/ds1"),
            archive_id: archive_id(),
        };
        let base_args = [
            "x",
            "/archives/ds1",
            "--archive-id",
            ARCHIVE_ID,
            "/staging",
            "--ordered",
        ];
        let cases = [
            (None, Vec::new()),
            (
                NonZeroU64::new(1024),
                vec!["--target-ordered-chunk-size", "1024"],
            ),
        ];

        for (target_chunk_size, chunk_size_args) in cases {
            let expected: Vec<OsString> = base_args
                .iter()
                .chain(chunk_size_args.iter())
                .chain(std::iter::once(&"--print-ordered-chunk-stats"))
                .map(OsString::from)
                .collect();
            assert_eq!(
                build_clp_s_extract_args(
                    &archive_descriptor,
                    Path::new("/staging"),
                    target_chunk_size
                ),
                expected,
                "target chunk size: {target_chunk_size:?}"
            );
        }
    }

    #[test]
    fn read_chunk_paths_parses_each_line_strictly() {
        let valid = b"{\"path\":\"/staging/a\"}\n{\"path\":\"/staging/b\"}\n";
        assert_eq!(
            read_chunk_paths(valid.as_slice()).expect("valid chunk statistics"),
            vec![PathBuf::from("/staging/a"), PathBuf::from("/staging/b")]
        );

        let over_long = [vec![b'x'; MAX_STATS_LINE_SIZE + 1], b"\n".to_vec()].concat();
        assert!(matches!(
            read_chunk_paths(b"{\"path\":\"/staging/a\"}".as_slice()),
            Err(ChunkStatsError::UnterminatedLine)
        ));
        assert!(matches!(
            read_chunk_paths(b"{\"path\":\"/staging/a\",\"size\":1}\n".as_slice()),
            Err(ChunkStatsError::MalformedLine(_))
        ));
        assert!(matches!(
            read_chunk_paths(b"not json\n".as_slice()),
            Err(ChunkStatsError::MalformedLine(_))
        ));
        assert!(matches!(
            read_chunk_paths(over_long.as_slice()),
            Err(ChunkStatsError::LineTooLong)
        ));
    }

    #[test]
    fn drain_tail_keeps_only_the_last_bytes() -> anyhow::Result<()> {
        const MAX_SIZE: usize = 100;

        let output: Vec<u8> = (0..3 * 8 * 1024 + 17_u32)
            .map(|index| u8::try_from(index % 251).expect("a value below 251 fits in a `u8`"))
            .collect();
        assert_eq!(
            drain_tail(output.as_slice(), MAX_SIZE)?,
            output[output.len() - MAX_SIZE..]
        );

        let short_output = b"short";
        assert_eq!(drain_tail(short_output.as_slice(), MAX_SIZE)?, short_output);
        Ok(())
    }

    #[test]
    fn run_clp_s_extract_kills_and_reaps_clp_s_on_an_over_long_stats_line() -> anyhow::Result<()> {
        const RETURN_TIMEOUT: Duration = Duration::from_secs(10);

        let clp_home = tempfile::tempdir()?;
        let pid_path = clp_home.path().join("pid");
        let bin_directory = clp_home.path().join("bin");
        std::fs::create_dir(&bin_directory)?;
        let clp_s_path = bin_directory.join("clp-s");
        let mut clp_s = std::fs::File::create(&clp_s_path)?;
        write!(
            clp_s,
            "#!/bin/sh\necho $$ > '{}'\nhead -c {} /dev/zero | tr '\\0' x\nexec sleep 60\n",
            pid_path.display(),
            MAX_STATS_LINE_SIZE + 1
        )?;
        clp_s.set_permissions(std::fs::Permissions::from_mode(0o755))?;
        drop(clp_s);

        let (sender, receiver) = std::sync::mpsc::channel();
        let home = clp_home.path().to_path_buf();
        std::thread::spawn(move || {
            let result = run_clp_s_extract(
                &home,
                &ArchiveDescriptor::Directory {
                    path: home.join("archives"),
                    archive_id: archive_id(),
                },
                &[],
                &home.join("staging"),
                None,
            );
            sender
                .send(result)
                .expect("the test should still be waiting for the result");
        });
        let error = receiver
            .recv_timeout(RETURN_TIMEOUT)
            .expect("run_clp_s_extract should return promptly")
            .expect_err("an over-long stats line should fail the extraction");

        assert!(
            matches!(
                error.downcast_ref::<ChunkStatsError>(),
                Some(ChunkStatsError::LineTooLong)
            ),
            "unexpected error: {error:#}"
        );
        let pid = std::fs::read_to_string(&pid_path)?;
        assert!(
            !Path::new("/proc").join(pid.trim()).exists(),
            "clp-s (pid {}) should have been reaped",
            pid.trim()
        );
        Ok(())
    }
}
