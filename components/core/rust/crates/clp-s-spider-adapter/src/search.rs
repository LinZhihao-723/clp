//! The in-process clp-s search task.

use std::num::NonZeroU16;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use clp_rust_utils::aws::AWS_DEFAULT_REGION;
use clp_rust_utils::clp_config::package::config::ArchiveOutputStorage;
use clp_rust_utils::clp_config::package::config::SpiderTaskExecutorConfig;
use clp_rust_utils::clp_config::package::config::StorageEngine;
use clp_rust_utils::dataset::resolve_dataset_name;
use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::job_config::SessionToken;
use clp_rust_utils::task_io::query::ClpSQueryOption;
use clp_rust_utils::task_io::query::OutputHandle;
use clp_rust_utils::task_io::query::QueryTaskIndex;
use clp_rust_utils::types::ArchiveId;
use clp_s_reader::ArchiveLocation;
use clp_s_reader::ByteSource;
use clp_s_reader::ErrorKind;
use clp_s_reader::Query;
use clp_s_reader::QueryOptions;
use clp_s_reader::ReadError;
use clp_s_reader::ResultOrder;
use clp_s_reader::ScanRequest;
use clp_s_reader::SearchSession;
use clp_s_reader::SessionReport;
use clp_s_reader::Sink;
use clp_s_reader::TimeBounds;
use clp_s_reader::s3::S3Location;
use clp_s_reader::s3::S3Source;
use clp_s_reader::s3::S3SourceConfig;
use clp_s_results_cache::ResultsCacheConfig;
use clp_s_results_cache::ResultsCacheSink;
use clp_s_sinks::NetworkDestination;
use clp_s_sinks::NetworkSink;
use non_empty_string::NonEmptyString;

use crate::AdapterResources;
use crate::admission::BudgetShortfall;
use crate::subprocess;
use crate::tracking::WriteTrackingSink;

/// The validated inputs of one search task.
#[derive(Debug)]
pub struct SearchTask<'task> {
    pub ctx: &'task spider_tdl::TaskContext,
    pub config: &'task SpiderTaskExecutorConfig,
    pub query_job_id: QueryJobId,
    pub clp_s_query_option: &'task ClpSQueryOption,
    pub archive_id: ArchiveId,

    /// The resolved dataset name.
    pub dataset: &'task str,

    pub output: TaskOutput<'task>,

    /// The task's position among the tasks of the query job.
    pub task_index: QueryTaskIndex,
}

impl<'task> SearchTask<'task> {
    /// Factory function.
    ///
    /// Validates the task inputs like `clp-tdl-package`'s `search` task and resolves the dataset
    /// name.
    ///
    /// # Returns
    ///
    /// The task on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if the package does not use the clp-s storage engine.
    /// * [`anyhow::Error`] if `output_handle` is neither the results cache nor the network.
    pub fn new(
        ctx: &'task spider_tdl::TaskContext,
        config: &'task SpiderTaskExecutorConfig,
        query_job_id: QueryJobId,
        clp_s_query_option: &'task ClpSQueryOption,
        archive: ArchiveToSearch<'task>,
        output_handle: &'task OutputHandle,
    ) -> anyhow::Result<Self> {
        if StorageEngine::ClpS != config.package.storage_engine {
            anyhow::bail!("the clp-s query task requires the `clp-s` storage engine");
        }
        let output = match output_handle {
            OutputHandle::ResultsCache { uri } => TaskOutput::ResultsCache { uri: uri.as_str() },
            OutputHandle::Network {
                host,
                port,
                session_token,
            } => TaskOutput::Network {
                host: host.as_str(),
                port: *port,
                session_token: *session_token,
            },
            OutputHandle::File => anyhow::bail!("unsupported query output handler"),
        };
        Ok(Self {
            ctx,
            config,
            query_job_id,
            clp_s_query_option,
            archive_id: archive.id,
            dataset: resolve_dataset_name(archive.dataset),
            output,
            task_index: archive.task_index,
        })
    }

    /// Searches the archive in-process, or with the `clp-s` subprocess if the reader does not
    /// support the archive.
    ///
    /// # Returns
    ///
    /// Whether the `clp-s` subprocess searched the archive, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if the task panics.
    /// * Forwards [`Self::resolve_and_search`]'s return values on failure.
    pub fn run(&self, resources: &AdapterResources) -> anyhow::Result<bool> {
        tracing::info!(
            job_id = % self.ctx.job_id,
            task_id = % self.ctx.task_id,
            task_instance_id = % self.ctx.task_instance_id,
            query_job_id = % self.query_job_id,
            dataset = % self.dataset,
            archive_id = % self.archive_id,
            task_index = self.task_index,
            "clp-s query task started.",
        );
        // spider-tdl does not catch panics at the task boundary, and a panicking task must not
        // take the execution manager down with it.
        let is_clp_s_subprocess_used =
            std::panic::catch_unwind(AssertUnwindSafe(|| self.resolve_and_search(resources)))
                .map_err(|_| anyhow::anyhow!("the clp-s query task panicked"))??;
        tracing::info!(
            job_id = % self.ctx.job_id,
            task_id = % self.ctx.task_id,
            task_instance_id = % self.ctx.task_instance_id,
            query_job_id = % self.query_job_id,
            is_clp_s_subprocess_used,
            "clp-s query task completed successfully.",
        );
        Ok(is_clp_s_subprocess_used)
    }

    /// Mirror of the body of `clp-tdl-package`'s `search` task after its input checks: resolves
    /// the archive input, builds the `clp-s` arguments for the task's output and runs `clp-s` from
    /// the CLP home.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`subprocess::resolve_archive_input`]'s return values on failure.
    /// * Forwards [`subprocess::run_clp_s_search`]'s return values on failure.
    pub fn run_clp_s_subprocess(&self, resources: &AdapterResources) -> anyhow::Result<()> {
        let (archive_selector, credential_env) = subprocess::resolve_archive_input(
            resources.runtime(),
            resources.clp_home(),
            self.config,
            self.dataset,
            self.archive_id,
        )
        .inspect_err(|e| {
            tracing::error!(
                job_id = % self.ctx.job_id,
                task_id = % self.ctx.task_id,
                task_instance_id = % self.ctx.task_instance_id,
                query_job_id = % self.query_job_id,
                error = % e,
                "Failed to resolve the archive input."
            );
        })?;
        let args = match self.output {
            TaskOutput::ResultsCache { uri } => {
                subprocess::build_clp_s_search_args_for_result_cache(
                    &archive_selector,
                    self.clp_s_query_option,
                    uri,
                    self.query_job_id,
                    self.dataset,
                )
            }
            TaskOutput::Network {
                host,
                port,
                session_token,
            } => subprocess::build_clp_s_search_args_for_network(
                &archive_selector,
                self.clp_s_query_option,
                host,
                port,
                session_token,
                self.task_index,
            ),
        };
        subprocess::run_clp_s_search(
            &subprocess::clp_binary_path(resources.clp_home(), "clp-s"),
            args,
            &credential_env,
        )
    }

    /// Resolves the archive location and the sink of the task's output (the results cache or a
    /// protocol-v2 network stream), then searches the archive.
    ///
    /// # Returns
    ///
    /// Whether the `clp-s` subprocess searched the archive, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`ResolvedLocation::new`]'s return values on failure.
    /// * Forwards [`AdapterResources::results_cache`]'s return values on failure.
    /// * Forwards [`ResultsCacheSink::new`]'s return values on failure.
    /// * Forwards [`Self::search_or_fall_back`]'s return values on failure.
    fn resolve_and_search(&self, resources: &AdapterResources) -> anyhow::Result<bool> {
        let location =
            ResolvedLocation::new(resources, self.config, self.dataset, &self.archive_id)
                .inspect_err(|e| {
                    tracing::error!(
                        job_id = % self.ctx.job_id,
                        task_id = % self.ctx.task_id,
                        task_instance_id = % self.ctx.task_instance_id,
                        query_job_id = % self.query_job_id,
                        error = % e,
                        "Failed to resolve the archive location."
                    );
                })?;
        match self.output {
            TaskOutput::ResultsCache { uri } => {
                let sink = ResultsCacheSink::new(
                    resources.runtime().clone(),
                    resources.results_cache(uri)?,
                    &self.query_job_id.to_string(),
                    self.dataset,
                    ResultsCacheConfig::default(),
                )?;
                self.search_or_fall_back(resources, &location, sink)
            }
            TaskOutput::Network {
                host,
                port,
                session_token,
            } => {
                let sink = NetworkSink::new(NetworkDestination {
                    host: host.to_owned(),
                    port: port.get(),
                    session_token: session_token.to_string(),
                    task_index: self.task_index,
                    archive_id: clp_s_reader::ArchiveId::from(self.archive_id.to_string()),
                });
                self.search_or_fall_back(resources, &location, sink)
            }
        }
    }

    /// Searches the archive at `location` in-process into `sink`; if the reader does not support
    /// the archive and nothing was handed to `sink` yet, searches it with the `clp-s` subprocess
    /// instead.
    ///
    /// # Type Parameters
    ///
    /// * `SinkType` - The sink receiving the in-process results.
    ///
    /// # Returns
    ///
    /// Whether the `clp-s` subprocess searched the archive, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::scan_request`]'s return values on failure.
    /// * Forwards [`Self::run_session`]'s return values on failure.
    /// * Forwards [`Self::run_clp_s_subprocess`]'s return values on failure.
    fn search_or_fall_back<SinkType: Sink>(
        &self,
        resources: &AdapterResources,
        location: &ResolvedLocation,
        sink: SinkType,
    ) -> anyhow::Result<bool> {
        let sink = Arc::new(WriteTrackingSink::new(sink));
        let request = Arc::new(self.scan_request()?);
        match self.run_session(resources, &request, location, &sink) {
            Ok(report) => {
                tracing::debug!(
                    query_job_id = % self.query_job_id,
                    summaries = ? report.archive_summaries,
                    "In-process search finished."
                );
                Ok(false)
            }
            Err(error) if ErrorKind::UnsupportedArchive == error.kind() && !sink.has_written() => {
                tracing::warn!(
                    query_job_id = % self.query_job_id,
                    archive_id = % self.archive_id,
                    error = % error,
                    "The reader does not support the archive; searching it with the clp-s \
                     subprocess."
                );
                self.run_clp_s_subprocess(resources)?;
                Ok(true)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Builds the scan request of the task: the KQL query (case-insensitive if requested), the
    /// time bounds, and the order (latest first with a result limit into the results cache, else
    /// scan order).
    ///
    /// # Returns
    ///
    /// The request on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Query::from_kql`]'s return values on failure.
    /// * Forwards [`clp_s_reader::ScanRequestBuilder::build`]'s return values on failure.
    fn scan_request(&self) -> Result<ScanRequest, ReadError> {
        let option = self.clp_s_query_option;
        let query = Query::from_kql(
            option.query_string.as_str(),
            &QueryOptions::default().with_case_sensitivity(!option.ignore_case),
        )?;
        ScanRequest::builder()
            .query(query)
            .time_bounds(TimeBounds::from_epoch_millis(
                option.begin_timestamp_millisecs,
                option.end_timestamp_millisecs,
            ))
            .order(match (self.output, option.max_num_results) {
                (TaskOutput::ResultsCache { .. }, Some(limit)) => {
                    ResultOrder::LatestFirst { limit }
                }
                (TaskOutput::ResultsCache { .. }, None) | (TaskOutput::Network { .. }, _) => {
                    ResultOrder::Scan
                }
            })
            .build()
    }

    /// Runs the search session over the archive with `WaitForBase` admission: a session that fails
    /// because the memory budget cannot admit a stream waits until the budget has room for it and
    /// runs again, until the admission timeout passes.
    ///
    /// # Type Parameters
    ///
    /// * `SinkType` - The sink receiving the results.
    ///
    /// # Returns
    ///
    /// The session report on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`clp_s_reader::SearchSessionBuilder::build`]'s return values on failure.
    /// * Forwards [`SearchSession::run`]'s return values on failure, including a memory-budget
    ///   exhaustion that outlasts the admission timeout or that no budget state can admit.
    fn run_session<SinkType: Sink>(
        &self,
        resources: &AdapterResources,
        request: &Arc<ScanRequest>,
        location: &ResolvedLocation,
        sink: &Arc<SinkType>,
    ) -> Result<SessionReport<SinkType::Output>, ReadError> {
        let reader_context = resources.reader_context();
        let timeout = Duration::from_secs(resources.base_admission_timeout_sec());
        let mut deadline = None;
        loop {
            let session = SearchSession::builder(Arc::clone(reader_context), Arc::clone(request))
                .archive(
                    clp_s_reader::ArchiveId::from(self.archive_id.to_string()),
                    location.to_archive_location(),
                )
                .build()?;
            let error = match session.run(Arc::clone(sink)) {
                Ok(report) => return Ok(report),
                Err(error) => error,
            };
            let Some(shortfall) =
                BudgetShortfall::from_error(&error).filter(BudgetShortfall::is_admissible)
            else {
                return Err(error);
            };
            let deadline = *deadline.get_or_insert_with(|| Instant::now() + timeout);
            if !shortfall.wait_for_room(reader_context.memory_budget(), deadline) {
                tracing::warn!(
                    query_job_id = % self.query_job_id,
                    archive_id = % self.archive_id,
                    requested_size = shortfall.requested_size,
                    timeout_sec = timeout.as_secs(),
                    "The memory budget did not admit the search before the admission timeout."
                );
                return Err(error);
            }
            tracing::debug!(
                query_job_id = % self.query_job_id,
                archive_id = % self.archive_id,
                requested_size = shortfall.requested_size,
                "Retrying the search now that the memory budget has room."
            );
        }
    }
}

/// The archive that a search task searches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArchiveToSearch<'task> {
    pub id: ArchiveId,

    /// The archive's dataset, or `None` for the default dataset.
    pub dataset: Option<&'task str>,

    /// The archive's position among the archives that the query job searches.
    pub task_index: QueryTaskIndex,
}

/// Where a search task writes its results.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskOutput<'task> {
    /// The results cache at `uri`; the collection is the query job's ID.
    ResultsCache { uri: &'task str },

    /// A search-result listener, streamed to with protocol v2.
    Network {
        host: &'task str,
        port: NonZeroU16,
        session_token: SessionToken,
    },
}

/// Searches one archive in-process and writes the results into the results cache or streams them
/// to a search-result listener.
///
/// Mirrors `clp-tdl-package`'s `search` task with the same inputs; the process-wide
/// [`AdapterResources`] replace the package's runtime and the spawned `clp-s` process. An archive
/// the reader does not support (`ErrorKind::UnsupportedArchive` before any result was written) is
/// searched by spawning the C++ `clp-s`, exactly as the package does today. Retries are idempotent:
/// the results cache tolerates duplicate `_id`s, and every attempt streams the same results with
/// the same indices, which the listener deduplicates.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`SearchTask::new`]'s return values on failure.
/// * Forwards [`AdapterResources::global`]'s return values on failure.
/// * Forwards [`SearchTask::run`]'s return values on failure.
pub fn search(
    ctx: &spider_tdl::TaskContext,
    config: &SpiderTaskExecutorConfig,
    query_job_id: QueryJobId,
    clp_s_query_option: &ClpSQueryOption,
    archive: ArchiveToSearch<'_>,
    output_handle: &OutputHandle,
) -> anyhow::Result<()> {
    let task = SearchTask::new(
        ctx,
        config,
        query_job_id,
        clp_s_query_option,
        archive,
        output_handle,
    )?;
    task.run(AdapterResources::global()?).map(drop)
}

/// Resolves where the archive lives: a single-file archive under the dataset's staging directory,
/// or an S3 object under the dataset's key prefix.
///
/// # Returns
///
/// The archive location on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`clp_s_reader::s3::S3Source::new`]'s return values on failure.
pub fn resolve_archive_location(
    resources: &AdapterResources,
    config: &SpiderTaskExecutorConfig,
    dataset: &str,
    archive_id: &ArchiveId,
) -> anyhow::Result<ArchiveLocation> {
    Ok(ResolvedLocation::new(resources, config, dataset, archive_id)?.to_archive_location())
}

/// An archive location that can be opened more than once.
enum ResolvedLocation {
    File(PathBuf),
    Source(Arc<dyn ByteSource>),
}

impl ResolvedLocation {
    /// Factory function.
    ///
    /// Resolves the archive like `clp-tdl-package`'s `resolve_archive_input`: on a file system,
    /// the file named by the archive id in the dataset's directory under the archive-output
    /// staging directory; on S3, the object named by the dataset's archive object key.
    ///
    /// # Returns
    ///
    /// The location on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`S3Source::new`]'s return values on failure.
    fn new(
        resources: &AdapterResources,
        config: &SpiderTaskExecutorConfig,
        dataset: &str,
        archive_id: &ArchiveId,
    ) -> anyhow::Result<Self> {
        let s3_config = match &config.archive_output.storage {
            ArchiveOutputStorage::Fs { .. } => {
                return Ok(Self::File(
                    config
                        .abs_archive_output_staging(resources.clp_home())
                        .join(dataset)
                        .join(archive_id.to_string()),
                ));
            }
            ArchiveOutputStorage::S3 { s3_config, .. } => s3_config,
        };
        let object_key = config
            .archive_output
            .dataset_archive_object_key(Some(dataset), archive_id);
        let region = s3_config
            .region_code
            .as_ref()
            .map_or(AWS_DEFAULT_REGION, NonEmptyString::as_str);
        let client = resources.s3_client(
            region,
            s3_config.endpoint_url.as_ref(),
            &s3_config.aws_authentication,
        );
        let source = S3Source::new(
            client,
            resources.runtime().clone(),
            S3Location {
                bucket: s3_config.bucket.as_str().to_owned(),
                key: object_key.as_str().to_owned(),
            },
            S3SourceConfig::default(),
        )?;
        Ok(Self::Source(Arc::new(source)))
    }

    /// # Returns
    ///
    /// The location as the reader names it.
    fn to_archive_location(&self) -> ArchiveLocation {
        match self {
            Self::File(path) => ArchiveLocation::File(path.clone()),
            Self::Source(source) => ArchiveLocation::Source(Arc::clone(source)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::io::Write;
    use std::net::SocketAddr;
    use std::net::TcpListener;
    use std::net::TcpStream;
    use std::num::NonZeroU16;
    use std::panic::AssertUnwindSafe;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use std::time::Instant;

    use clp_rust_utils::clp_config::AwsAuthentication;
    use clp_rust_utils::clp_config::AwsCredentials;
    use clp_rust_utils::clp_config::S3Config;
    use clp_rust_utils::clp_config::package::config::ArchiveOutput;
    use clp_rust_utils::clp_config::package::config::ArchiveOutputStorage;
    use clp_rust_utils::clp_config::package::config::Package;
    use clp_rust_utils::clp_config::package::config::SpiderTaskExecutorConfig;
    use clp_rust_utils::clp_config::package::config::StorageEngine;
    use clp_rust_utils::task_io::query::ClpSQueryOption;
    use clp_rust_utils::task_io::query::OutputHandle;
    use clp_rust_utils::types::ArchiveId;
    use clp_rust_utils::types::non_empty_string::ExpectedNonEmpty;
    use clp_s_object_store::test_util::MockS3Server;
    use clp_s_reader::Archive;
    use clp_s_reader::ArchiveLocation;
    use clp_s_reader::ErrorKind;
    use clp_s_reader::MergeOrder;
    use clp_s_reader::OutputRequirements;
    use clp_s_reader::PartialSink;
    use clp_s_reader::PartitionInfo;
    use clp_s_reader::PartitionSummary;
    use clp_s_reader::ReadError;
    use clp_s_reader::ReaderContext;
    use clp_s_reader::RecordBatch;
    use clp_s_reader::ResultOrder;
    use clp_s_reader::Sink;
    use clp_s_reader::SinkControl;
    use clp_s_reader::SinkError;
    use clp_s_reader::TimeBounds;
    use non_empty_string::NonEmptyString;
    use spider_core::types::id::JobId;
    use spider_core::types::id::ResourceGroupId;
    use spider_core::types::id::TaskId;
    use spider_tdl::TaskContext;

    use super::ArchiveToSearch;
    use super::ResolvedLocation;
    use super::SearchTask;
    use super::TaskOutput;
    use super::resolve_archive_location;
    use super::search;
    use crate::AdapterConfig;
    use crate::AdapterResources;

    const ARCHIVE_ID: &str = "018e90e5-8b2a-4a61-a2fc-cac799936caf";
    const DATASET: &str = "ds1";
    const QUERY_JOB_ID: i32 = 7;
    const TASK_INDEX: u64 = 3;
    const SESSION_TOKEN: &str = "6f1c1c64-1e0a-4be8-a7a6-6a0e4d3c2b1a";
    const BUCKET: &str = "bucket";

    /// A lazily connected results cache that no test reaches: searches without matches never
    /// insert, and every other search fails before inserting.
    const UNREACHABLE_URI: &str = "mongodb://127.0.0.1:1/proto?serverSelectionTimeoutMS=200";

    /// One decoded handshake: `(version, session_token, task_index, archive_id)`.
    type Handshake = (u8, String, u64, String);

    /// One decoded result frame: `(result_index, timestamp_ms, message)`.
    type Frame = (u64, i64, String);

    /// A CLP home whose `bin/clp-s` records its arguments and its AWS access key instead of
    /// searching, and whose archive-output directory is `archives`.
    struct TestHome {
        directory: tempfile::TempDir,
        runtime: tokio::runtime::Runtime,
    }

    impl TestHome {
        /// Factory function.
        ///
        /// Creates a CLP home with the fake `clp-s` and an empty dataset directory, and a
        /// two-thread runtime.
        ///
        /// # Returns
        ///
        /// The newly created test home.
        ///
        /// # Panics
        ///
        /// Panics if the home or the runtime can't be created.
        fn new() -> Self {
            use std::os::unix::fs::PermissionsExt;

            let directory = tempfile::tempdir().expect("a temporary directory can be created");
            let bin = directory.path().join("bin");
            std::fs::create_dir_all(&bin).expect("the bin directory can be created");
            let clp_s = bin.join("clp-s");
            std::fs::write(
                &clp_s,
                "#!/bin/sh\nhome=\"$(dirname \"$0\")/..\"\nprintf '%s\\n' \"$@\" \
                 \"AWS_ACCESS_KEY_ID=$AWS_ACCESS_KEY_ID\" > \"$home/clp-s.args\"\n",
            )
            .expect("the fake clp-s can be written");
            std::fs::set_permissions(&clp_s, std::fs::Permissions::from_mode(0o755))
                .expect("the fake clp-s can be made executable");
            std::fs::create_dir_all(directory.path().join("archives").join(DATASET))
                .expect("the dataset directory can be created");
            Self {
                directory,
                runtime: tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("a multi-thread runtime can be built"),
            }
        }

        /// # Returns
        ///
        /// The CLP home directory.
        fn path(&self) -> &Path {
            self.directory.path()
        }

        /// # Returns
        ///
        /// The adapter resources of this home, with `reader_context` and a one-second base
        /// admission timeout.
        ///
        /// # Panics
        ///
        /// Panics if [`AdapterResources::new`] fails.
        fn resources(&self, reader_context: Arc<ReaderContext>) -> AdapterResources {
            AdapterResources::new(
                self.runtime.handle().clone(),
                reader_context,
                AdapterConfig {
                    clp_home: self.path().to_path_buf(),
                    base_admission_timeout_sec: 1,
                },
            )
            .expect("the runtime is multi-threaded")
        }

        /// Links the fixture archive (or archive directory) `relative_path` as the test archive.
        fn stage(&self, relative_path: &str) {
            std::os::unix::fs::symlink(
                fixture(relative_path),
                self.path().join("archives").join(DATASET).join(ARCHIVE_ID),
            )
            .expect("the archive can be staged");
        }

        /// # Returns
        ///
        /// The lines the fake `clp-s` recorded, or `None` if it never ran.
        fn clp_s_args(&self) -> Option<Vec<String>> {
            std::fs::read_to_string(self.path().join("clp-s.args"))
                .ok()
                .map(|text| text.lines().map(str::to_owned).collect())
        }
    }

    /// How [`FaultySink`] fails.
    #[derive(Clone, Copy, Debug)]
    enum Fault {
        UnsupportedArchiveOnOpen,
        UnsupportedArchiveOnConsume,
        PanicOnConsume,
    }

    /// A sink that fails with [`Fault`].
    #[derive(Debug)]
    struct FaultySink(Fault);

    impl Sink for FaultySink {
        type Output = ();
        type Partial = FaultyPartial;

        fn requirements(&self) -> OutputRequirements {
            OutputRequirements::json()
        }

        fn merge_order(&self) -> MergeOrder {
            MergeOrder::Scan
        }

        fn open_partial(&self, _partition: &PartitionInfo) -> Result<FaultyPartial, SinkError> {
            match self.0 {
                Fault::UnsupportedArchiveOnOpen => Err(unsupported_archive_error()),
                Fault::UnsupportedArchiveOnConsume | Fault::PanicOnConsume => {
                    Ok(FaultyPartial(self.0))
                }
            }
        }

        fn merge(&self, _output: &mut (), _partial: ()) -> Result<SinkControl, SinkError> {
            Ok(SinkControl::Continue)
        }
    }

    /// The partial sink of [`FaultySink`].
    #[derive(Debug)]
    struct FaultyPartial(Fault);

    impl PartialSink for FaultyPartial {
        type Output = ();

        fn consume(&mut self, _batch: &RecordBatch<'_>) -> Result<SinkControl, SinkError> {
            match self.0 {
                Fault::UnsupportedArchiveOnConsume => Err(unsupported_archive_error()),
                Fault::PanicOnConsume => panic!("the sink panics"),
                Fault::UnsupportedArchiveOnOpen => Ok(SinkControl::Continue),
            }
        }

        fn finish(self, _summary: &PartitionSummary) -> Result<(), SinkError> {
            Ok(())
        }
    }

    /// # Returns
    ///
    /// A sink error of kind `UnsupportedArchive`: the error of opening a multi-file archive.
    fn unsupported_archive_error() -> SinkError {
        let error = Archive::open(
            ReaderContext::builder()
                .build()
                .expect("the default context is valid"),
            "mfa".into(),
            ArchiveLocation::File(fixture("reject/scalars.mfa")),
        )
        .expect_err("multi-file archives are unsupported");
        assert_eq!(error.kind(), ErrorKind::UnsupportedArchive);
        SinkError::Read(Box::new(error))
    }

    /// # Returns
    ///
    /// The path of the fixture archive `relative_path`.
    fn fixture(relative_path: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/archives")
            .join(relative_path)
    }

    /// # Returns
    ///
    /// The id of the test archive.
    fn archive_id() -> ArchiveId {
        ARCHIVE_ID.parse().expect("valid archive UUID")
    }

    /// # Returns
    ///
    /// The test archive as task `TASK_INDEX` of the job searches it, in `dataset`.
    fn archive(dataset: Option<&str>) -> ArchiveToSearch<'_> {
        ArchiveToSearch {
            id: archive_id(),
            dataset,
            task_index: TASK_INDEX,
        }
    }

    /// # Returns
    ///
    /// An executor config for clp-s archives stored on the file system under `archives`.
    fn fs_config() -> SpiderTaskExecutorConfig {
        SpiderTaskExecutorConfig {
            package: Package {
                storage_engine: StorageEngine::ClpS,
                ..Package::default()
            },
            archive_output: ArchiveOutput {
                storage: ArchiveOutputStorage::Fs {
                    directory: "archives".to_owned(),
                },
                ..ArchiveOutput::default()
            },
            ..SpiderTaskExecutorConfig::default()
        }
    }

    /// # Returns
    ///
    /// An executor config for clp-s archives stored in S3 at `endpoint_url`, with static
    /// credentials.
    fn s3_config(endpoint_url: &str) -> SpiderTaskExecutorConfig {
        SpiderTaskExecutorConfig {
            archive_output: ArchiveOutput {
                storage: ArchiveOutputStorage::S3 {
                    staging_directory: "staging".to_owned(),
                    s3_config: S3Config {
                        bucket: NonEmptyString::from_static_str(BUCKET),
                        region_code: None,
                        key_prefix: NonEmptyString::from_static_str("LIB1/"),
                        endpoint_url: Some(
                            NonEmptyString::new(endpoint_url.to_owned())
                                .expect("the endpoint is not empty"),
                        ),
                        aws_authentication: AwsAuthentication::Credentials {
                            credentials: AwsCredentials {
                                access_key_id: "the-access-key".to_owned(),
                                secret_access_key: "the-secret-key".to_owned(),
                                session_token: None,
                            },
                        },
                    },
                },
                ..ArchiveOutput::default()
            },
            ..fs_config()
        }
    }

    /// # Returns
    ///
    /// The query options of `kql`, without time bounds, a result limit or case-insensitivity.
    fn query_option(kql: &'static str) -> ClpSQueryOption {
        ClpSQueryOption {
            query_string: NonEmptyString::from_static_str(kql),
            max_num_results: None,
            begin_timestamp_millisecs: None,
            end_timestamp_millisecs: None,
            ignore_case: false,
        }
    }

    /// # Returns
    ///
    /// The context of a non-commit task of a random job.
    fn task_context() -> TaskContext {
        TaskContext::new(
            JobId::random(),
            TaskId::Index(0),
            1,
            ResourceGroupId::random(),
            None,
        )
        .expect("a non-commit task context without graph outputs is valid")
    }

    /// # Returns
    ///
    /// An output handle to the results cache at `uri`.
    fn results_cache(uri: &'static str) -> OutputHandle {
        OutputHandle::ResultsCache {
            uri: NonEmptyString::from_static_str(uri),
        }
    }

    /// # Returns
    ///
    /// An output handle streaming to a listener at `address` in session [`SESSION_TOKEN`].
    fn network(address: SocketAddr) -> OutputHandle {
        OutputHandle::Network {
            host: NonEmptyString::new(address.ip().to_string()).expect("the host is not empty"),
            port: NonZeroU16::new(address.port()).expect("the bound port is not zero"),
            session_token: SESSION_TOKEN.parse().expect("valid session token UUID"),
        }
    }

    /// Accepts the connections to `listener`, reads each to its end, and acknowledges the results
    /// its trailer counts if `is_acknowledging`, until a connection closes without sending
    /// anything.
    ///
    /// # Returns
    ///
    /// A handle of the thread returning the bytes received on each non-empty connection.
    fn capture(
        listener: TcpListener,
        is_acknowledging: bool,
    ) -> std::thread::JoinHandle<Vec<Vec<u8>>> {
        std::thread::spawn(move || {
            let mut connections = Vec::new();
            while let Ok((mut stream, _)) = listener.accept() {
                let mut received = Vec::new();
                stream
                    .read_to_end(&mut received)
                    .expect("the connection can be read");
                if received.is_empty() {
                    break;
                }
                if is_acknowledging {
                    let num_results = decode_stream(&received)
                        .expect("the connection carries a stream")
                        .2
                        .expect("the stream ends with a trailer");
                    let mut ack = vec![0x91, 0xcf];
                    ack.extend_from_slice(&num_results.to_be_bytes());
                    stream
                        .write_all(&ack)
                        .expect("the acknowledgement can be sent");
                }
                connections.push(received);
            }
            connections
        })
    }

    /// Decodes the bytes of one connection.
    ///
    /// # Returns
    ///
    /// A tuple on success, containing:
    ///
    /// * The handshake.
    /// * The result frames.
    /// * The trailer's count, if the connection ends with a trailer.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`rmp_serde::decode::from_read`]'s return values on failure.
    fn decode_stream(data: &[u8]) -> anyhow::Result<(Handshake, Vec<Frame>, Option<u64>)> {
        let mut remaining = data;
        let handshake = rmp_serde::decode::from_read(&mut remaining)?;
        let mut frames = Vec::new();
        while !remaining.is_empty() {
            if 0x91 == remaining[0] {
                let (num_results,): (u64,) = rmp_serde::decode::from_read(&mut remaining)?;
                return Ok((handshake, frames, Some(num_results)));
            }
            frames.push(rmp_serde::decode::from_read(&mut remaining)?);
        }
        Ok((handshake, frames, None))
    }

    /// Runs one task over the test archive with `output_handle`.
    ///
    /// # Returns
    ///
    /// Whether the `clp-s` subprocess searched the archive, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`SearchTask::new`]'s return values on failure.
    /// * Forwards [`SearchTask::run`]'s return values on failure.
    fn run_with_output(
        resources: &AdapterResources,
        clp_s_query_option: &ClpSQueryOption,
        output_handle: &OutputHandle,
    ) -> anyhow::Result<bool> {
        let ctx = task_context();
        let config = fs_config();
        SearchTask::new(
            &ctx,
            &config,
            QUERY_JOB_ID,
            clp_s_query_option,
            archive(Some(DATASET)),
            output_handle,
        )?
        .run(resources)
    }

    /// Runs one task over the test archive.
    ///
    /// # Returns
    ///
    /// Whether the `clp-s` subprocess searched the archive, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`SearchTask::new`]'s return values on failure.
    /// * Forwards [`SearchTask::run`]'s return values on failure.
    fn run(
        resources: &AdapterResources,
        config: &SpiderTaskExecutorConfig,
        clp_s_query_option: &ClpSQueryOption,
    ) -> anyhow::Result<bool> {
        let ctx = task_context();
        let output_handle = results_cache(UNREACHABLE_URI);
        SearchTask::new(
            &ctx,
            config,
            QUERY_JOB_ID,
            clp_s_query_option,
            archive(Some(DATASET)),
            &output_handle,
        )?
        .run(resources)
    }

    /// # Returns
    ///
    /// The decoded size of the largest stream of fixture `relative_path` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`ReaderContextBuilder::build`]'s return values on failure.
    /// * Forwards [`Archive::open`]'s return values on failure.
    fn largest_stream_size(relative_path: &str) -> anyhow::Result<u64> {
        let archive = Archive::open(
            ReaderContext::builder().build()?,
            "archive".into(),
            ArchiveLocation::File(fixture(relative_path)),
        )?;
        Ok(archive
            .tables()
            .iter()
            .map(|table| table.stream_offset + table.uncompressed_size)
            .max()
            .unwrap_or_default())
    }

    /// # Returns
    ///
    /// The kind of `error` if it is a [`ReadError`], or `None` otherwise.
    fn read_error_kind(error: &anyhow::Error) -> Option<ErrorKind> {
        error.downcast_ref::<ReadError>().map(ReadError::kind)
    }

    #[test]
    fn task_inputs_are_validated_before_anything_else() {
        let option = query_option("level: ERROR");
        let error = search(
            &task_context(),
            &SpiderTaskExecutorConfig::default(),
            QUERY_JOB_ID,
            &option,
            archive(None),
            &results_cache(UNREACHABLE_URI),
        )
        .expect_err("the clp storage engine is unsupported");
        assert!(error.to_string().contains("clp-s"), "{error}");
        let error = search(
            &task_context(),
            &fs_config(),
            QUERY_JOB_ID,
            &option,
            archive(None),
            &OutputHandle::File,
        )
        .expect_err("the file output handler is unsupported");
        assert!(error.to_string().contains("unsupported"), "{error}");
    }

    #[test]
    fn datasets_default_and_archives_resolve_under_the_staging_directory() -> anyhow::Result<()> {
        let home = TestHome::new();
        let resources = home.resources(ReaderContext::builder().build()?);
        let ctx = task_context();
        let option = query_option("*");
        let output_handle = results_cache(UNREACHABLE_URI);
        let config = fs_config();
        let task = SearchTask::new(
            &ctx,
            &config,
            QUERY_JOB_ID,
            &option,
            archive(None),
            &output_handle,
        )?;
        assert_eq!(task.dataset, "default");
        assert_eq!(
            task.output,
            TaskOutput::ResultsCache {
                uri: UNREACHABLE_URI
            }
        );
        assert_eq!(task.task_index, TASK_INDEX);

        let location = resolve_archive_location(&resources, &config, DATASET, &archive_id())?;
        let ArchiveLocation::File(path) = location else {
            panic!("expected a file location, got {location:?}");
        };
        assert_eq!(
            path,
            home.path().join("archives").join(DATASET).join(ARCHIVE_ID)
        );

        let absolute = SpiderTaskExecutorConfig {
            archive_output: ArchiveOutput {
                storage: ArchiveOutputStorage::Fs {
                    directory: "/absolute".to_owned(),
                },
                ..ArchiveOutput::default()
            },
            ..fs_config()
        };
        let location = resolve_archive_location(&resources, &absolute, DATASET, &archive_id())?;
        let ArchiveLocation::File(path) = location else {
            panic!("expected a file location, got {location:?}");
        };
        assert_eq!(
            path,
            PathBuf::from("/absolute").join(DATASET).join(ARCHIVE_ID)
        );
        Ok(())
    }

    #[test]
    fn supported_archives_are_searched_in_process() -> anyhow::Result<()> {
        let home = TestHome::new();
        home.stage("search.ts.clps");
        let resources = home.resources(ReaderContext::builder().build()?);
        assert!(!run(
            &resources,
            &fs_config(),
            &query_option("no_such_key: 1")
        )?);
        assert_eq!(home.clp_s_args(), None);

        let error = run(&resources, &fs_config(), &query_option("*"))
            .expect_err("the results cache is unreachable");
        assert_eq!(read_error_kind(&error), Some(ErrorKind::Sink), "{error:?}");
        assert_eq!(home.clp_s_args(), None, "sink failures never fall back");
        Ok(())
    }

    #[test]
    fn unsupported_archives_are_searched_by_clp_s() -> anyhow::Result<()> {
        for relative_path in [
            "reject/scalars.mfa",
            "reject/arrays.structurize-arrays.clps",
        ] {
            let home = TestHome::new();
            home.stage(relative_path);
            let resources = home.resources(ReaderContext::builder().build()?);
            let option = ClpSQueryOption {
                begin_timestamp_millisecs: Some(5),
                ignore_case: true,
                ..query_option("a: 1")
            };
            assert!(run(&resources, &fs_config(), &option)?, "{relative_path}");
            let staging = home.path().join("archives").join(DATASET);
            let staging = staging.to_string_lossy();
            let args = home.clp_s_args();
            assert_eq!(
                args.as_deref()
                    .and_then(|args| args.split_last())
                    .map(|(_, args)| args),
                Some(
                    [
                        "s",
                        staging.as_ref(),
                        "--archive-id",
                        ARCHIVE_ID,
                        "a: 1",
                        "--tge",
                        "5",
                        "--ignore-case",
                        "results-cache",
                        "--uri",
                        UNREACHABLE_URI,
                        "--collection",
                        "7",
                        "--dataset",
                        DATASET,
                    ]
                    .map(str::to_owned)
                    .as_slice()
                ),
                "{relative_path}"
            );
        }
        Ok(())
    }

    #[test]
    fn other_failures_never_fall_back() -> anyhow::Result<()> {
        let home = TestHome::new();
        let resources = home.resources(ReaderContext::builder().build()?);
        let error = run(&resources, &fs_config(), &query_option("*"))
            .expect_err("the archive does not exist");
        assert_eq!(read_error_kind(&error), Some(ErrorKind::Io), "{error:?}");

        home.stage("search.ts.clps");
        let error = run(&resources, &fs_config(), &query_option("a: (b"))
            .expect_err("the query is invalid");
        assert!(
            error
                .downcast_ref::<ReadError>()
                .is_some_and(|error| ErrorKind::InvalidQuery == error.kind()),
            "{error:?}"
        );
        assert_eq!(home.clp_s_args(), None);
        Ok(())
    }

    #[test]
    fn s3_archives_are_searched_in_process_and_fall_back_with_credentials() -> anyhow::Result<()> {
        let home = TestHome::new();
        let server = MockS3Server::start(home.runtime.handle())?;
        let key = format!("LIB1/{DATASET}/{ARCHIVE_ID}");
        let config = s3_config(server.endpoint_url());
        let resources = home.resources(ReaderContext::builder().build()?);

        server.put_object(BUCKET, &key, std::fs::read(fixture("search.ts.clps"))?);
        let location = resolve_archive_location(&resources, &config, DATASET, &archive_id())?;
        let archive = Archive::open(
            ReaderContext::builder().build()?,
            ARCHIVE_ID.into(),
            location,
        )?;
        assert_eq!(archive.info().num_log_events, 40);
        assert!(!run(&resources, &config, &query_option("no_such_key: 1"))?);
        assert_eq!(home.clp_s_args(), None);
        let requests = server.requests();
        assert!(
            requests
                .iter()
                .all(|request| BUCKET == request.bucket && key == request.key),
            "{requests:?}"
        );

        server.put_object(
            BUCKET,
            &key,
            std::fs::read(fixture("reject/arrays.structurize-arrays.clps"))?,
        );
        assert!(run(&resources, &config, &query_option("a: 1"))?);
        let url = format!("{}/{BUCKET}/{key}", server.endpoint_url());
        assert_eq!(
            home.clp_s_args().as_deref().map(|args| &args[..4]),
            Some(
                ["s", url.as_str(), "--auth", "s3"]
                    .map(str::to_owned)
                    .as_slice()
            )
        );
        assert_eq!(
            home.clp_s_args()
                .and_then(|args| args.last().cloned())
                .as_deref(),
            Some("AWS_ACCESS_KEY_ID=the-access-key")
        );
        Ok(())
    }

    #[test]
    fn base_admission_waits_for_the_budget_to_drain() -> anyhow::Result<()> {
        let stream_size = largest_stream_size("search.ts.clps")?;
        let home = TestHome::new();
        home.stage("search.ts.clps");
        let reader_context = ReaderContext::builder()
            .memory_budget_size(Some(stream_size + 10))
            .build()?;
        let resources = home.resources(Arc::clone(&reader_context));
        let option = query_option("n: 424242");

        assert!(!run(&resources, &fs_config(), &option)?);

        let held = reader_context.memory_budget().try_reserve(20)?;
        let begin = Instant::now();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        assert!(!run(&resources, &fs_config(), &option)?);
        assert!(begin.elapsed() >= Duration::from_millis(300));
        release
            .join()
            .map_err(|_| anyhow::anyhow!("the release thread panicked"))?;

        let _held = reader_context.memory_budget().try_reserve(20)?;
        let begin = Instant::now();
        let error = run(&resources, &fs_config(), &option)
            .expect_err("the budget stays full past the admission timeout");
        assert_eq!(
            read_error_kind(&error),
            Some(ErrorKind::ResourceExhausted),
            "{error:?}"
        );
        assert!(begin.elapsed() >= Duration::from_secs(1));
        assert_eq!(home.clp_s_args(), None);
        Ok(())
    }

    #[test]
    fn streams_larger_than_the_budget_fail_without_waiting() -> anyhow::Result<()> {
        let home = TestHome::new();
        home.stage("search.ts.clps");
        let resources = home.resources(
            ReaderContext::builder()
                .memory_budget_size(Some(16))
                .build()?,
        );
        let begin = Instant::now();
        let error = run(&resources, &fs_config(), &query_option("n: 424242"))
            .expect_err("no stream fits in 16 bytes");
        assert_eq!(
            read_error_kind(&error),
            Some(ErrorKind::ResourceExhausted),
            "{error:?}"
        );
        assert!(begin.elapsed() < Duration::from_secs(1));
        Ok(())
    }

    #[test]
    fn falling_back_requires_that_nothing_was_written() -> anyhow::Result<()> {
        let config = fs_config();
        let option = query_option("*");
        let ctx = task_context();
        let output_handle = results_cache(UNREACHABLE_URI);
        let task = SearchTask::new(
            &ctx,
            &config,
            QUERY_JOB_ID,
            &option,
            archive(Some(DATASET)),
            &output_handle,
        )?;
        let run = |fault| {
            let home = TestHome::new();
            home.stage("search.ts.clps");
            let resources = home.resources(ReaderContext::builder().build()?);
            let location = ResolvedLocation::new(&resources, &config, DATASET, &archive_id())?;
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                task.search_or_fall_back(&resources, &location, FaultySink(fault))
            }));
            anyhow::Ok((result, home.clp_s_args().is_some()))
        };
        let panicked = |_| anyhow::anyhow!("the search panicked");

        let (result, has_clp_s_run) = run(Fault::UnsupportedArchiveOnOpen)?;
        assert!(
            result.map_err(panicked)??,
            "nothing was written, so the task falls back"
        );
        assert!(has_clp_s_run);

        let (result, has_clp_s_run) = run(Fault::UnsupportedArchiveOnConsume)?;
        let error = result
            .map_err(panicked)?
            .expect_err("a batch was handed to the sink, so there is no fallback");
        assert_eq!(
            read_error_kind(&error),
            Some(ErrorKind::UnsupportedArchive),
            "{error:?}"
        );
        assert!(!has_clp_s_run);

        let (result, has_clp_s_run) = run(Fault::PanicOnConsume)?;
        assert!(result.is_err(), "the panic is not turned into a fallback");
        assert!(!has_clp_s_run);
        Ok(())
    }

    #[test]
    fn panics_anywhere_in_the_task_are_errors() -> anyhow::Result<()> {
        let home = TestHome::new();
        home.stage("search.ts.clps");
        let resources = home.resources(ReaderContext::builder().build()?);
        // Connecting to the results cache blocks on the runtime, which panics inside an
        // asynchronous execution context, before the in-process search starts.
        let async_context = tokio::runtime::Builder::new_current_thread().build()?;
        let error = async_context
            .block_on(async { run(&resources, &fs_config(), &query_option("*")) })
            .expect_err("the panic is caught");
        assert!(error.to_string().contains("panicked"), "{error}");
        assert_eq!(home.clp_s_args(), None);
        Ok(())
    }

    #[test]
    fn supported_archives_are_streamed_in_process() -> anyhow::Result<()> {
        let home = TestHome::new();
        home.stage("search.ts.clps");
        let resources = home.resources(ReaderContext::builder().build()?);
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let connections = capture(listener, true);
        assert!(!run_with_output(
            &resources,
            &query_option("level: WARN"),
            &network(address)
        )?);
        assert!(!run_with_output(
            &resources,
            &query_option("no_such_key: 1"),
            &network(address)
        )?);
        drop(TcpStream::connect(address)?);
        let connections = connections
            .join()
            .map_err(|_| anyhow::anyhow!("the capture thread panicked"))?;
        assert_eq!(home.clp_s_args(), None);
        assert_eq!(
            connections.len(),
            1,
            "searches without matches never connect"
        );

        let (handshake, frames, num_results) = decode_stream(&connections[0])?;
        assert_eq!(
            handshake,
            (
                2,
                SESSION_TOKEN.to_owned(),
                TASK_INDEX,
                ARCHIVE_ID.to_owned()
            )
        );
        let mut result_indices = Vec::new();
        for (result_index, timestamp_ms, message) in frames {
            assert!(timestamp_ms > 0, "{message}");
            assert!(message.ends_with("}\n"), "{message}");
            result_indices.push(result_index);
        }
        assert!(result_indices.len() > 1);
        assert_eq!(
            result_indices,
            (0..result_indices.len() as u64).collect::<Vec<_>>()
        );
        assert_eq!(num_results, Some(result_indices.len() as u64));
        Ok(())
    }

    #[test]
    fn in_process_streams_fail_without_an_acknowledgement() -> anyhow::Result<()> {
        let home = TestHome::new();
        home.stage("search.ts.clps");
        let resources = home.resources(ReaderContext::builder().build()?);
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let connections = capture(listener, false);
        let error = run_with_output(&resources, &query_option("level: WARN"), &network(address))
            .expect_err("the listener closes without acknowledging");
        drop(TcpStream::connect(address)?);
        let connections = connections
            .join()
            .map_err(|_| anyhow::anyhow!("the capture thread panicked"))?;
        assert!(
            matches!(
                error.downcast_ref::<ReadError>(),
                Some(ReadError::Sink(sink)) if matches!(**sink, SinkError::DownstreamClosed)
            ),
            "{error:?}"
        );
        assert_eq!(home.clp_s_args(), None);
        assert_eq!(connections.len(), 1);
        let (_, frames, num_results) = decode_stream(&connections[0])?;
        assert_eq!(num_results, Some(frames.len() as u64));
        Ok(())
    }

    #[test]
    fn unsupported_archives_are_streamed_by_clp_s() -> anyhow::Result<()> {
        let home = TestHome::new();
        home.stage("reject/scalars.mfa");
        let resources = home.resources(ReaderContext::builder().build()?);
        let address = SocketAddr::from(([127, 0, 0, 1], 4242));
        let option = ClpSQueryOption {
            max_num_results: std::num::NonZeroU32::new(5),
            end_timestamp_millisecs: Some(9),
            ..query_option("a: 1")
        };
        assert!(run_with_output(&resources, &option, &network(address))?);
        let staging = home.path().join("archives").join(DATASET);
        let staging = staging.to_string_lossy();
        let task_index = TASK_INDEX.to_string();
        assert_eq!(
            home.clp_s_args()
                .as_deref()
                .and_then(|args| args.split_last())
                .map(|(_, args)| args),
            Some(
                [
                    "s",
                    staging.as_ref(),
                    "--archive-id",
                    ARCHIVE_ID,
                    "a: 1",
                    "--tle",
                    "9",
                    "network",
                    "--host",
                    "127.0.0.1",
                    "--port",
                    "4242",
                    "--session-token",
                    SESSION_TOKEN,
                    "--task-index",
                    task_index.as_str(),
                ]
                .map(str::to_owned)
                .as_slice()
            )
        );
        Ok(())
    }

    #[test]
    fn requests_carry_the_time_bounds_and_the_result_limit() -> anyhow::Result<()> {
        let config = fs_config();
        let ctx = task_context();
        let output_handle = results_cache(UNREACHABLE_URI);
        let network_handle = network(SocketAddr::from(([127, 0, 0, 1], 4242)));
        let limit = std::num::NonZeroU32::new(5).expect("5 is nonzero");
        let limited = ClpSQueryOption {
            max_num_results: Some(limit),
            begin_timestamp_millisecs: Some(10),
            end_timestamp_millisecs: Some(20),
            ..query_option("a: 1")
        };
        for (option, output_handle, order) in [
            (query_option("a: 1"), &output_handle, ResultOrder::Scan),
            (
                limited.clone(),
                &output_handle,
                ResultOrder::LatestFirst { limit },
            ),
            (query_option("a: 1"), &network_handle, ResultOrder::Scan),
            (limited, &network_handle, ResultOrder::Scan),
        ] {
            let request = SearchTask::new(
                &ctx,
                &config,
                QUERY_JOB_ID,
                &option,
                archive(Some(DATASET)),
                output_handle,
            )?
            .scan_request()?;
            assert_eq!(request.order(), order);
            assert_eq!(
                request.time_bounds(),
                TimeBounds::from_epoch_millis(
                    option.begin_timestamp_millisecs,
                    option.end_timestamp_millisecs
                )
            );
        }
        Ok(())
    }
}
