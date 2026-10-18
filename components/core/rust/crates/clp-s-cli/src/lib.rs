//! The library behind `clp-s-rs`, the command-line front end of the Rust clp-s reader (`extract`
//! and `search`): a thin clap layer over `clp_s_reader::SearchSession`.

pub mod args;
pub mod exit;
#[cfg(feature = "s3")]
mod s3;

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use clp_s_reader::ArchiveId;
use clp_s_reader::ArchiveLocation;
use clp_s_reader::ColumnPattern;
use clp_s_reader::PruneReason;
use clp_s_reader::Query;
use clp_s_reader::QueryOptions;
use clp_s_reader::ReadError;
use clp_s_reader::ReaderContext;
use clp_s_reader::RecordProjection;
use clp_s_reader::ResultOrder;
use clp_s_reader::ScanRequest;
use clp_s_reader::SearchSession;
use clp_s_reader::Sink;
use clp_s_reader::TimeBounds;
use clp_s_sinks::JsonLinesSink;
use clp_s_sinks::MsgpackResultSink;

use crate::args::ArchiveArgs;
use crate::args::Auth;
use crate::args::Cli;
use crate::args::Command;
use crate::args::ExtractArgs;
use crate::args::OutputKind;
use crate::args::SearchArgs;

/// The `OUTPUT_DIR` of `extract` that writes the records to stdout instead of a file.
pub const STDOUT_OUTPUT_DIR: &str = "-";

/// Parses the process arguments, runs the subcommand and maps its outcome to an exit code. Usage
/// errors exit with [`exit::EXIT_INVALID_ARGUMENT`], not clap's default of 2.
///
/// # Returns
///
/// The process exit code.
#[must_use]
pub fn run() -> ExitCode {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    if tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_env_filter(filter)
        .try_init()
        .is_err()
    {
        eprintln!("clp-s-rs: warning: a global logger is already installed");
    }
    ExitCode::from(execute(std::env::args_os()))
}

/// A file opened for appending (and created if missing) on its first write or flush.
#[derive(Debug)]
struct LazyFile {
    path: PathBuf,
    file: Option<std::fs::File>,
}

impl LazyFile {
    /// # Returns
    ///
    /// The file, opened on the first call, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`std::fs::OpenOptions::open`]'s return values on failure.
    fn file(&mut self) -> std::io::Result<&mut std::fs::File> {
        let file = match self.file.take() {
            Some(file) => file,
            None => std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?,
        };
        Ok(self.file.insert(file))
    }
}

impl std::io::Write for LazyFile {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.file()?.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file()?.flush()
    }
}

/// Why a subcommand failed.
#[derive(Debug)]
enum CommandError {
    InvalidArgument(String),

    Input {
        path: PathBuf,
        source: std::io::Error,
    },

    Output {
        path: PathBuf,
        source: std::io::Error,
    },

    /// The network output cannot connect to its peer.
    Connect {
        address: String,
        source: std::io::Error,
    },

    /// The asynchronous runtime of the results cache or of S3 inputs cannot be started.
    #[cfg(any(feature = "results-cache", feature = "s3"))]
    Runtime(std::io::Error),

    /// An S3 input is not an object URL or cannot be opened.
    #[cfg(feature = "s3")]
    S3(clp_s_reader::s3::ObjectStoreError),

    Read(ReadError),
}

impl CommandError {
    /// # Returns
    ///
    /// The process exit code of the failure.
    fn exit_code(&self) -> u8 {
        match self {
            Self::InvalidArgument(_) => exit::EXIT_INVALID_ARGUMENT,
            #[cfg(any(feature = "results-cache", feature = "s3"))]
            Self::Runtime(_) => exit::EXIT_INTERNAL,
            #[cfg(feature = "s3")]
            Self::S3(error) => exit::exit_code(error.kind()),
            Self::Input { .. } => exit::EXIT_IO,
            Self::Output { .. } | Self::Connect { .. } => exit::EXIT_SINK,
            Self::Read(error) => exit::exit_code(error.kind()),
        }
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidArgument(message) => formatter.write_str(message),
            Self::Input { path, source } => {
                write!(formatter, "cannot read `{}`: {source}", path.display())
            }
            Self::Output { path, source } => {
                write!(formatter, "cannot write to `{}`: {source}", path.display())
            }
            Self::Connect { address, source } => {
                write!(formatter, "cannot connect to `{address}`: {source}")
            }
            #[cfg(any(feature = "results-cache", feature = "s3"))]
            Self::Runtime(source) => write!(formatter, "cannot start the runtime: {source}"),
            #[cfg(feature = "s3")]
            Self::S3(error) => formatter.write_str(&error_chain_message(error)),
            Self::Read(error) => formatter.write_str(&error_chain_message(error)),
        }
    }
}

impl From<ReadError> for CommandError {
    fn from(error: ReadError) -> Self {
        Self::Read(error)
    }
}

/// The archives of one invocation, in argument order.
struct Inputs {
    archives: Vec<(ArchiveId, ArchiveLocation)>,

    /// The runtime the S3 archives read through; it must outlive every read of them.
    #[cfg(feature = "s3")]
    _s3: Option<s3::S3Inputs>,
}

/// Parses `args` (including the program name), runs the subcommand and reports a failure on
/// stderr.
///
/// # Type Parameters
///
/// * `ArgsType` - The arguments.
///
/// # Returns
///
/// The exit code.
fn execute<ArgsType: IntoIterator<Item = OsString>>(args: ArgsType) -> u8 {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let is_usage_error = error.use_stderr();
            if error.print().is_err() {
                return exit::EXIT_INVALID_ARGUMENT;
            }
            return if is_usage_error {
                exit::EXIT_INVALID_ARGUMENT
            } else {
                exit::EXIT_SUCCESS
            };
        }
    };
    let result = match cli.command {
        Command::Extract(args) => extract(&args),
        Command::Search(args) => search(&args),
    };
    match result {
        Ok(()) => exit::EXIT_SUCCESS,
        Err(error) => {
            eprintln!("clp-s-rs: error: {error}");
            error.exit_code()
        }
    }
}

/// Decompresses every input archive, unordered, appending JSON lines to `OUTPUT_DIR/original`
/// (or writing them to stdout for [`STDOUT_OUTPUT_DIR`]).
///
/// # Errors
///
/// Returns an error if:
///
/// * [`CommandError::InvalidArgument`] if `--ordered` is given.
/// * [`CommandError::Output`] if the output directory cannot be created.
/// * Forwards [`resolve_archives`]'s return values on failure.
/// * Forwards [`clp_s_reader::ReaderContextBuilder::build`]'s return values on failure.
/// * Forwards [`clp_s_reader::ScanRequestBuilder::build`]'s return values on failure.
/// * Forwards [`extract_each`]'s return values on failure.
fn extract(args: &ExtractArgs) -> Result<(), CommandError> {
    if args.ordered {
        return Err(CommandError::InvalidArgument(
            "ordered decompression (`--ordered`) is not supported by this reader".to_owned(),
        ));
    }
    let inputs = resolve_archives(&args.archives)?;
    let context = ReaderContext::builder().build()?;
    let request = Arc::new(ScanRequest::builder().build()?);
    if STDOUT_OUTPUT_DIR == args.output_dir.as_os_str() {
        let sink = Arc::new(JsonLinesSink::new(std::io::BufWriter::new(
            std::io::stdout(),
        )));
        return extract_each(inputs.archives, &context, &request, &sink);
    }
    std::fs::create_dir_all(&args.output_dir).map_err(|source| CommandError::Output {
        path: args.output_dir.clone(),
        source,
    })?;
    let sink = Arc::new(JsonLinesSink::new(std::io::BufWriter::new(LazyFile {
        path: args.output_dir.join("original"),
        file: None,
    })));
    extract_each(inputs.archives, &context, &request, &sink)
}

/// Decompresses each of `archives` into `sink` with a session of its own, in order.
///
/// # Type Parameters
///
/// * `WriterType` - The destination of the records.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`clp_s_reader::SearchSessionBuilder::build`]'s return values on failure.
/// * Forwards [`SearchSession::run`]'s return values on failure.
fn extract_each<WriterType: std::io::Write + Send + 'static>(
    archives: Vec<(ArchiveId, ArchiveLocation)>,
    context: &Arc<ReaderContext>,
    request: &Arc<ScanRequest>,
    sink: &Arc<JsonLinesSink<WriterType>>,
) -> Result<(), CommandError> {
    for (archive_id, location) in archives {
        SearchSession::builder(Arc::clone(context), Arc::clone(request))
            .archive(archive_id, location)
            .build()?
            .run(Arc::clone(sink))?;
    }
    Ok(())
}

/// Searches every input archive as one session, in argument order, writing the matches to the
/// requested output: JSON lines on stdout, msgpack result tuples into a file or to a network peer
/// (opened once per session), or documents into the results cache. With `--max-num-results`,
/// only the session-wide latest matches are written. Archives that cannot match because the query
/// is logically false or because they have no timestamp column to bound get a warning, not an
/// error (DV-20).
///
/// # Errors
///
/// Returns an error if:
///
/// * [`CommandError::InvalidArgument`] if an aggregation (`--count`, `--count-by-time`) is
///   requested, or an output lacks its arguments.
/// * [`CommandError::Output`] if the output file cannot be created.
/// * [`CommandError::Connect`] if the network peer cannot be reached.
/// * Forwards [`search_request`]'s return values on failure.
/// * Forwards [`resolve_archives`]'s return values on failure.
/// * Forwards [`clp_s_reader::ReaderContextBuilder::build`]'s return values on failure.
/// * Forwards [`clp_s_reader::SearchSessionBuilder::build`]'s return values on failure.
/// * Forwards [`run_search`]'s return values on failure.
/// * Forwards [`search_results_cache`]'s return values on failure.
fn search(args: &SearchArgs) -> Result<(), CommandError> {
    if args.count || args.count_by_time.is_some() {
        return Err(CommandError::InvalidArgument(
            "aggregations (`--count`, `--count-by-time`) are not supported by this reader"
                .to_owned(),
        ));
    }
    let request = Arc::new(search_request(args)?);
    let inputs = resolve_archives(&args.archives)?;
    let context = ReaderContext::builder().build()?;
    let session = inputs
        .archives
        .into_iter()
        .fold(
            SearchSession::builder(context, request),
            |builder, (archive_id, location)| builder.archive(archive_id, location),
        )
        .build()?;
    match args.output {
        OutputKind::Stdout => run_search(
            session,
            JsonLinesSink::new(std::io::BufWriter::new(std::io::stdout())),
        ),
        OutputKind::File => {
            let path = required(args.path.as_ref(), "--path", "file")?;
            let file = std::fs::File::create(path).map_err(|source| CommandError::Output {
                path: path.clone(),
                source,
            })?;
            run_search(session, MsgpackResultSink::new(file))
        }
        OutputKind::Network => {
            let host = required(args.host.as_ref(), "--host", "network")?;
            let port = required(args.port.as_ref(), "--port", "network")?;
            let stream =
                std::net::TcpStream::connect((host.as_str(), *port)).map_err(|source| {
                    CommandError::Connect {
                        address: format!("{host}:{port}"),
                        source,
                    }
                })?;
            run_search(session, MsgpackResultSink::new(stream))
        }
        OutputKind::ResultsCache => search_results_cache(args, session),
    }
}

/// Runs `session` into the results cache named by `--uri` and `--collection`, from a
/// multi-thread runtime of its own.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`CommandError::InvalidArgument`] if `--uri` or `--collection` is missing or invalid, or the
///   batch size is 0.
/// * [`CommandError::Runtime`] if the runtime cannot be started.
/// * Forwards [`run_search`]'s return values on failure.
#[cfg(feature = "results-cache")]
fn search_results_cache(args: &SearchArgs, session: SearchSession) -> Result<(), CommandError> {
    use clp_s_results_cache::ResultsCacheConfig;
    use clp_s_results_cache::ResultsCacheSink;

    let uri = required(args.uri.as_ref(), "--uri", "results-cache")?;
    let collection = required(args.collection.as_ref(), "--collection", "results-cache")?;
    if collection.is_empty() {
        return Err(CommandError::InvalidArgument(
            "`--collection` cannot be empty".to_owned(),
        ));
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(CommandError::Runtime)?;
    let invalid = |error: clp_s_results_cache::ResultsCacheError| {
        CommandError::InvalidArgument(format!("results cache: {error}"))
    };
    let database = clp_s_results_cache::connect_database(runtime.handle(), uri).map_err(invalid)?;
    let sink = ResultsCacheSink::new(
        runtime.handle().clone(),
        database,
        collection,
        &args.dataset,
        ResultsCacheConfig {
            batch_size: args.batch_size,
        },
    )
    .map_err(invalid)?;
    run_search(session, sink)
}

/// Rejects the results-cache output of a binary built without the `results-cache` feature.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`CommandError::InvalidArgument`] always.
#[cfg(not(feature = "results-cache"))]
fn search_results_cache(_args: &SearchArgs, _session: SearchSession) -> Result<(), CommandError> {
    Err(CommandError::InvalidArgument(
        "this binary was built without the `results-cache` feature".to_owned(),
    ))
}

/// Runs `session` into `sink` and warns about archives that could not match.
///
/// # Type Parameters
///
/// * `SinkType` - The sink of the requested output.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`SearchSession::run`]'s return values on failure.
fn run_search<SinkType: Sink>(session: SearchSession, sink: SinkType) -> Result<(), CommandError> {
    let report = session.run(Arc::new(sink))?;
    for summary in &report.archive_summaries {
        match summary.prune_reason {
            Some(PruneReason::LogicallyFalse) => {
                tracing::warn!(
                    archive_id = % summary.archive_id,
                    "The query is logically false; no record can match."
                );
            }
            Some(PruneReason::NoTimestampColumn) => {
                tracing::warn!(
                    archive_id = % summary.archive_id,
                    "The archive has no authoritative timestamp column; the time bounds match no \
                     record."
                );
            }
            _ => {}
        }
    }
    Ok(())
}

/// # Type Parameters
///
/// * `ValueType` - The argument's value.
///
/// # Returns
///
/// The value of the argument `flag`, which `output` requires, on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`CommandError::InvalidArgument`] if `value` is `None`.
fn required<'value, ValueType>(
    value: Option<&'value ValueType>,
    flag: &str,
    output: &str,
) -> Result<&'value ValueType, CommandError> {
    value.ok_or_else(|| {
        CommandError::InvalidArgument(format!("`--output {output}` requires `{flag}`"))
    })
}

/// Builds the scan request of a search: the KQL query (case-insensitive with `--ignore-case`),
/// the time bounds and the projection, in C++ scan order, or latest first with
/// `--max-num-results`.
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
/// * Forwards [`ColumnPattern::parse`]'s return values on failure.
/// * Forwards [`clp_s_reader::ScanRequestBuilder::build`]'s return values on failure.
fn search_request(args: &SearchArgs) -> Result<ScanRequest, CommandError> {
    let options = QueryOptions::default().with_case_sensitivity(!args.ignore_case);
    let query = Query::from_kql(&args.query, &options).map_err(ReadError::from)?;
    let projection = if args.projection.is_empty() {
        RecordProjection::Full
    } else {
        RecordProjection::Columns(
            args.projection
                .iter()
                .map(|column| ColumnPattern::parse(column))
                .collect::<Result<_, _>>()
                .map_err(ReadError::from)?,
        )
    };
    Ok(ScanRequest::builder()
        .query(query)
        .time_bounds(TimeBounds::from_epoch_millis(
            args.time_begin_ms,
            args.time_end_ms,
        ))
        .projection(projection)
        .order(
            args.max_num_results
                .map_or(ResultOrder::Scan, |limit| ResultOrder::LatestFirst {
                    limit,
                }),
        )
        .build()?)
}

/// Resolves the archive inputs into the archives of one session, in argument order: a file is a
/// single-file archive; a directory is a multi-file archive (rejected when opened) if it holds
/// only multi-file archive members, and otherwise holds one archive per entry, sorted by name
/// (`--archive-id` selects one). With `--auth s3`, an input that does not exist locally is an S3
/// object URL (C++ `get_source_for_path`).
///
/// # Returns
///
/// The archives on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`CommandError::InvalidArgument`] if an input has no archive id, or `--auth s3` is given to a
///   binary built without the `s3` feature.
/// * [`CommandError::Input`] if a directory cannot be listed.
/// * Forwards [`s3::S3Inputs::new`]'s return values on failure.
/// * Forwards [`s3::S3Inputs::open`]'s return values on failure.
fn resolve_archives(args: &ArchiveArgs) -> Result<Inputs, CommandError> {
    let mut archives = Vec::new();
    #[cfg(feature = "s3")]
    let mut s3_inputs: Option<s3::S3Inputs> = None;
    for input in &args.archive_paths {
        let path = PathBuf::from(input);
        if Auth::S3 == args.auth && !path.exists() {
            #[cfg(feature = "s3")]
            {
                let s3_inputs = match &s3_inputs {
                    Some(s3_inputs) => s3_inputs,
                    None => s3_inputs.insert(s3::S3Inputs::new(
                        args.s3_region.clone(),
                        args.s3_endpoint_url.clone(),
                    )?),
                };
                archives.push(s3_inputs.open(input)?);
                continue;
            }
            #[cfg(not(feature = "s3"))]
            return Err(CommandError::InvalidArgument(
                "this binary was built without the `s3` feature".to_owned(),
            ));
        }
        for path in resolve_local_input(path, args.archive_id.as_deref())? {
            let archive_id = archive_id_of(&path).ok_or_else(|| {
                CommandError::InvalidArgument(format!(
                    "cannot derive an archive id from `{}`",
                    path.display()
                ))
            })?;
            archives.push((archive_id, ArchiveLocation::File(path)));
        }
    }
    Ok(Inputs {
        archives,
        #[cfg(feature = "s3")]
        _s3: s3_inputs,
    })
}

/// Resolves a local input: a file, a multi-file archive directory, or a directory of archives
/// (sorted by name, or only `archive_id` in it).
///
/// # Returns
///
/// The archive paths on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`CommandError::Input`] if a directory cannot be listed.
fn resolve_local_input(
    path: PathBuf,
    archive_id: Option<&str>,
) -> Result<Vec<PathBuf>, CommandError> {
    if !path.is_dir() {
        return Ok(vec![path]);
    }
    if let Some(archive_id) = archive_id {
        return Ok(vec![path.join(archive_id)]);
    }
    let input_error = |source| CommandError::Input {
        path: path.clone(),
        source,
    };
    let mut entries = std::fs::read_dir(&path)
        .map_err(input_error)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(input_error)?;
    if is_multi_file_archive(&entries) {
        return Ok(vec![path]);
    }
    entries.sort();
    Ok(entries)
}

/// Mirrors C++ `is_multi_file_archive` (`clp_s/Utils.cpp`): every entry is a regular member
/// file of a multi-file archive (a fixed section name or a numeric segment).
///
/// # Returns
///
/// Whether the directory entries `entries` are those of a multi-file archive.
fn is_multi_file_archive(entries: &[PathBuf]) -> bool {
    const MEMBER_NAMES: [&str; 7] = [
        "header",
        "schema_tree",
        "schema_ids",
        "var.dict",
        "log.dict",
        "array.dict",
        "table_metadata",
    ];
    entries.iter().all(|entry| {
        if entry.is_dir() {
            return false;
        }
        entry
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                MEMBER_NAMES.contains(&name)
                    || (!name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit()))
            })
    })
}

/// Explains `error` with every cause in its source chain. Wrapping errors repeat their source's
/// text, so only causes the message does not already contain are appended.
///
/// # Returns
///
/// The message.
fn error_chain_message(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !message.contains(&cause_text) {
            message.push_str(": ");
            message.push_str(&cause_text);
        }
        source = cause.source();
    }
    message
}

/// # Returns
///
/// The archive id of `path`: its last non-empty component, like C++
/// `get_last_non_empty_path_component`, or `None` if it has none.
fn archive_id_of(path: &Path) -> Option<ArchiveId> {
    path.components()
        .rev()
        .find_map(|component| match component {
            Component::Normal(name) => Some(ArchiveId::from(name.to_string_lossy().into_owned())),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::path::PathBuf;

    use super::archive_id_of;
    use super::is_multi_file_archive;

    #[test]
    fn archive_ids_are_the_last_non_empty_component() {
        for (path, expected) in [
            ("a/b/logs.clps", Some("logs.clps")),
            ("logs.clps", Some("logs.clps")),
            ("a/dir/", Some("dir")),
            ("a/dir/.", Some("dir")),
            ("/", None),
            ("", None),
        ] {
            assert_eq!(
                archive_id_of(Path::new(path))
                    .as_ref()
                    .map(clp_s_reader::ArchiveId::as_str),
                expected,
                "{path}"
            );
        }
    }

    #[test]
    fn multi_file_archive_directories_are_recognized() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let member = |name: &str| -> anyhow::Result<PathBuf> {
            let path = directory.path().join(name);
            std::fs::write(&path, b"")?;
            Ok(path)
        };
        let members = vec![member("header")?, member("schema_tree")?, member("0")?];
        assert!(is_multi_file_archive(&members));
        let mut mixed = members;
        mixed.push(member("logs.clps")?);
        assert!(!is_multi_file_archive(&mixed));
        let subdirectory = directory.path().join("nested");
        std::fs::create_dir(&subdirectory)?;
        assert!(!is_multi_file_archive(&[subdirectory]));
        Ok(())
    }
}
