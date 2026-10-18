//! The Rust reader as a [`ReaderUnderTest`]: the library (extract through its pull cursor, search
//! through a session into a JSON-lines or msgpack sink), and the `clp-s-rs` binary.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use clp_s_reader::Archive;
use clp_s_reader::ArchiveId;
use clp_s_reader::ArchiveLocation;
use clp_s_reader::CancellationToken;
use clp_s_reader::ColumnPattern;
use clp_s_reader::JsonOptions;
use clp_s_reader::OutputRequirements;
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

use crate::cpp::exit_code;
use crate::cpp::run_command;
use crate::reader::ExtractOutcome;
use crate::reader::OutputRequest;
use crate::reader::ReaderFailure;
use crate::reader::ReaderUnderTest;
use crate::reader::SearchOutcome;
use crate::reader::SearchOutput;
use crate::reader::SearchRequest;
use crate::reader::WireTuple;

/// The file name of the `clp-s-rs` binary.
pub const RUST_CLI_BINARY_NAME: &str = "clp-s-rs";

/// The Rust reader library, decompressing through [`Archive::scan`] (the pull cursor) and
/// searching through a [`SearchSession`] into a [`JsonLinesSink`] or a [`MsgpackResultSink`].
#[derive(Debug)]
pub struct RustLibraryReader {
    context: Arc<ReaderContext>,
    json_options: JsonOptions,
}

impl RustLibraryReader {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A reader with the default execution configuration rendering with `json_options`, on
    /// success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`clp_s_reader::ReaderContextBuilder::build`]'s return values on failure.
    pub fn new(json_options: JsonOptions) -> anyhow::Result<Self> {
        Ok(Self {
            context: ReaderContext::builder().build()?,
            json_options,
        })
    }

    /// Decompresses `archive` into `jsonl`, which keeps the records delivered before a failure.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Archive::open`]'s return values on failure.
    /// * Forwards [`clp_s_reader::ScanRequestBuilder::build`]'s return values on failure.
    /// * Forwards [`Archive::scan`]'s return values on failure.
    /// * Forwards [`clp_s_reader::Scan::next_batch`]'s return values on failure.
    fn extract_into(&self, archive: &Path, jsonl: &mut Vec<u8>) -> Result<(), ReadError> {
        let archive = Archive::open(
            Arc::clone(&self.context),
            archive_id_of(archive),
            ArchiveLocation::File(archive.to_path_buf()),
        )?;
        let request = ScanRequest::builder()
            .outputs(OutputRequirements::json())
            .json_options(self.json_options)
            .build()?;
        let mut scan = archive.scan(Arc::new(request), CancellationToken::new())?;
        while let Some(batch) = scan.next_batch()? {
            let records = batch.json().ok_or_else(|| {
                ReadError::Internal("a batch of a JSON request has no records".into())
            })?;
            jsonl.extend_from_slice(records.as_jsonl());
        }
        Ok(())
    }

    /// Searches the archives of `request` as one session, in order, writing the matches into
    /// `output` (JSON lines, or msgpack tuples for [`OutputRequest::Msgpack`]), which keeps the
    /// bytes written before a failure.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Query::from_kql`]'s return values on failure.
    /// * Forwards [`ColumnPattern::parse`]'s return values on failure.
    /// * Forwards [`clp_s_reader::ScanRequestBuilder::build`]'s return values on failure.
    /// * Forwards [`clp_s_reader::SearchSessionBuilder::build`]'s return values on failure.
    /// * Forwards [`run_into_buffer`]'s return values on failure.
    fn search_into(
        &self,
        request: &SearchRequest<'_>,
        output: &mut Vec<u8>,
    ) -> Result<(), ReadError> {
        let options = request.options;
        let query = Query::from_kql(
            request.kql,
            &QueryOptions::default().with_case_sensitivity(!options.is_case_insensitive),
        )?;
        let projection = if options.projection.is_empty() {
            RecordProjection::Full
        } else {
            RecordProjection::Columns(
                options
                    .projection
                    .iter()
                    .map(|column| ColumnPattern::parse(column))
                    .collect::<Result<_, _>>()?,
            )
        };
        let scan_request = ScanRequest::builder()
            .query(query)
            .time_bounds(TimeBounds::from_epoch_millis(
                options.time_begin_ms,
                options.time_end_ms,
            ))
            .projection(projection)
            .order(ResultOrder::Scan)
            .json_options(self.json_options)
            .build()?;
        let session = request
            .archives
            .iter()
            .fold(
                SearchSession::builder(Arc::clone(&self.context), Arc::new(scan_request)),
                |builder, archive| {
                    builder.archive(
                        archive_id_of(archive),
                        ArchiveLocation::File(archive.clone()),
                    )
                },
            )
            .build()?;
        if OutputRequest::Msgpack == request.output {
            run_into_buffer(
                session,
                MsgpackResultSink::new(Vec::new()),
                |sink| sink.into_writer().ok(),
                output,
            )
        } else {
            run_into_buffer(
                session,
                JsonLinesSink::new(Vec::new()),
                |sink| sink.into_writer().ok(),
                output,
            )
        }
    }
}

impl ReaderUnderTest for RustLibraryReader {
    fn name(&self) -> &'static str {
        "rust-library"
    }

    fn extract(&self, archive: &Path, _work_dir: &Path) -> anyhow::Result<ExtractOutcome> {
        let mut jsonl = Vec::new();
        let failure = self
            .extract_into(archive, &mut jsonl)
            .err()
            .map(|error| ReaderFailure {
                kind: Some(error.kind()),
                exit_code: None,
                message: error.to_string(),
            });
        Ok(ExtractOutcome { jsonl, failure })
    }

    fn search(
        &self,
        request: &SearchRequest<'_>,
        _work_dir: &Path,
    ) -> anyhow::Result<SearchOutcome> {
        if matches!(request.output, OutputRequest::ResultsCache { .. }) {
            return Ok(SearchOutcome {
                output: SearchOutput::Unsupported,
                failure: None,
            });
        }
        let mut written = Vec::new();
        let failure = self
            .search_into(request, &mut written)
            .err()
            .map(|error| ReaderFailure {
                kind: Some(error.kind()),
                exit_code: None,
                message: error.to_string(),
            });
        let output = if OutputRequest::Msgpack == request.output {
            SearchOutput::Msgpack(WireTuple::decode_stream(&written)?)
        } else {
            SearchOutput::JsonLines(written)
        };
        Ok(SearchOutcome { output, failure })
    }
}

/// The `clp-s-rs` binary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RustCliReader {
    binary: PathBuf,
}

impl RustCliReader {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A reader that runs the `clp-s-rs` binary at `binary`.
    #[must_use]
    pub const fn new(binary: PathBuf) -> Self {
        Self { binary }
    }
}

impl ReaderUnderTest for RustCliReader {
    fn name(&self) -> &'static str {
        "rust-cli"
    }

    fn extract(&self, archive: &Path, work_dir: &Path) -> anyhow::Result<ExtractOutcome> {
        let output_dir = work_dir.join("x");
        let mut command = Command::new(&self.binary);
        command.arg("x").arg(archive).arg("-o").arg(&output_dir);
        let output = run_command(&mut command)?;
        let original = output_dir.join("original");
        let jsonl = if original.exists() {
            std::fs::read(&original)?
        } else {
            Vec::new()
        };
        let exit_code = exit_code(output.status);
        let failure = (0 != exit_code).then(|| ReaderFailure {
            kind: None,
            exit_code: Some(exit_code),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
        Ok(ExtractOutcome { jsonl, failure })
    }

    fn search(
        &self,
        request: &SearchRequest<'_>,
        work_dir: &Path,
    ) -> anyhow::Result<SearchOutcome> {
        if matches!(request.output, OutputRequest::ResultsCache { .. }) {
            return Ok(SearchOutcome {
                output: SearchOutput::Unsupported,
                failure: None,
            });
        }
        let options = request.options;
        let mut command = Command::new(&self.binary);
        command
            .arg("s")
            .args(request.archives)
            .arg(format!("--query={}", request.kql));
        if options.is_case_insensitive {
            command.arg("--ignore-case");
        }
        if let Some(begin) = options.time_begin_ms {
            command.arg(format!("--tge={begin}"));
        }
        if let Some(end) = options.time_end_ms {
            command.arg(format!("--tle={end}"));
        }
        if !options.projection.is_empty() {
            command.arg("--projection").args(&options.projection);
        }
        let msgpack_path = work_dir.join("results.msgpack");
        if OutputRequest::Msgpack == request.output {
            command
                .args(["--output", "file", "--path"])
                .arg(&msgpack_path);
        }
        let output = run_command(&mut command)?;
        let exit_code = exit_code(output.status);
        let failure = (0 != exit_code).then(|| ReaderFailure {
            kind: None,
            exit_code: Some(exit_code),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
        let output = if OutputRequest::Msgpack != request.output {
            SearchOutput::JsonLines(output.stdout)
        } else if msgpack_path.exists() {
            SearchOutput::Msgpack(WireTuple::decode_stream(&std::fs::read(&msgpack_path)?)?)
        } else {
            SearchOutput::Msgpack(Vec::new())
        };
        Ok(SearchOutcome { output, failure })
    }
}

/// Runs `session` into `sink`, then appends to `output` what the sink wrote into its buffer, which
/// keeps the bytes written before a failure.
///
/// # Type Parameters
///
/// * `SinkType` - The sink, writing into a `Vec<u8>`.
/// * `IntoBufferType` - Takes the buffer back from the sink.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`ReadError::Internal`] if the sink's buffer is still shared after the session.
/// * Forwards [`SearchSession::run`]'s return values on failure.
fn run_into_buffer<SinkType: Sink, IntoBufferType: FnOnce(SinkType) -> Option<Vec<u8>>>(
    session: SearchSession,
    sink: SinkType,
    into_buffer: IntoBufferType,
    output: &mut Vec<u8>,
) -> Result<(), ReadError> {
    let sink = Arc::new(sink);
    let result = session.run(Arc::clone(&sink));
    let buffer = Arc::try_unwrap(sink)
        .ok()
        .and_then(into_buffer)
        .ok_or_else(|| {
            ReadError::Internal("the sink's buffer is still shared after a session".into())
        })?;
    output.extend_from_slice(&buffer);
    result.map(drop)
}

/// # Returns
///
/// The archive id of `archive`: its file name (C++ parity), or the whole path if it has none.
fn archive_id_of(archive: &Path) -> ArchiveId {
    archive.file_name().map_or_else(
        || ArchiveId::from(archive.to_string_lossy().into_owned()),
        |name| ArchiveId::from(name.to_string_lossy().into_owned()),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clp_s_reader::ErrorKind;
    use clp_s_reader::JsonOptions;

    use super::RustLibraryReader;
    use super::archive_id_of;
    use crate::canonical::canonical_digest;
    use crate::corpus::prototype_root;
    use crate::reader::OutputRequest;
    use crate::reader::ReaderUnderTest;
    use crate::reader::SearchOptions;
    use crate::reader::SearchOutput;
    use crate::reader::SearchRequest;
    use crate::reader::WireTuple;

    #[test]
    fn archive_ids_are_file_names() {
        assert_eq!(
            archive_id_of(Path::new("a/b/logs.ts.clps")).as_str(),
            "logs.ts.clps"
        );
        assert_eq!(archive_id_of(Path::new("/")).as_str(), "/");
    }

    #[test]
    fn library_extract_reproduces_a_fixture_oracle() -> anyhow::Result<()> {
        let fixtures = prototype_root().join("tests/fixtures");
        let reader = RustLibraryReader::new(JsonOptions::default())?;
        let work = tempfile::tempdir()?;
        let outcome = reader.extract(
            &fixtures.join("archives/many-schemas.tables256.clps"),
            work.path(),
        )?;
        assert_eq!(outcome.failure, None);
        let oracle = std::fs::read(fixtures.join("oracles/many-schemas.tables256.jsonl"))?;
        assert_eq!(canonical_digest(&outcome.jsonl), canonical_digest(&oracle));
        Ok(())
    }

    #[test]
    fn library_extract_reports_the_error_kind() -> anyhow::Result<()> {
        let reader = RustLibraryReader::new(JsonOptions::default())?;
        let work = tempfile::tempdir()?;
        let missing = reader.extract(&work.path().join("missing.clps"), work.path())?;
        assert_eq!(
            missing.failure.and_then(|failure| failure.kind),
            Some(ErrorKind::Io)
        );
        let garbage = work.path().join("garbage.clps");
        std::fs::write(&garbage, vec![0_u8; 128])?;
        let corrupt = reader.extract(&garbage, work.path())?;
        assert_eq!(
            corrupt.failure.and_then(|failure| failure.kind),
            Some(ErrorKind::CorruptArchive)
        );
        assert_eq!(corrupt.jsonl, Vec::<u8>::new());
        Ok(())
    }

    #[test]
    fn library_search_reproduces_query_oracles() -> anyhow::Result<()> {
        let root = prototype_root();
        let archives = root.join("tests/fixtures/archives");
        let oracles = root.join("tests/queries/oracles");
        let reader = RustLibraryReader::new(JsonOptions::default())?;
        let work = tempfile::tempdir()?;
        let split: Vec<_> = (0..9)
            .map(|index| archives.join(format!("logs.ts-split.{index}.clps")))
            .collect();
        let single = [archives.join("search.ts.clps")];
        let projected = SearchOptions {
            projection: vec!["rid".to_owned(), "level".to_owned()],
            ..SearchOptions::default()
        };
        let time_bounded = SearchOptions {
            time_begin_ms: Some(1_700_000_030_000),
            ..SearchOptions::default()
        };
        let case_insensitive = SearchOptions {
            is_case_insensitive: true,
            ..SearchOptions::default()
        };
        for (id, archives, kql, options) in [
            (
                "logs-split-var",
                &split[..],
                "level: ERROR AND service: auth",
                SearchOptions::default(),
            ),
            ("ci-var", &single[..], "level: error", case_insensitive),
            (
                "time-lower-and-filter",
                &single[..],
                "level: INFO",
                time_bounded,
            ),
            ("proj-basic", &single[..], "level: WARN", projected),
        ] {
            let outcome = reader.search(
                &SearchRequest {
                    archives,
                    kql,
                    options: &options,
                    output: OutputRequest::Stdout,
                },
                work.path(),
            )?;
            assert_eq!(outcome.failure, None, "{id}");
            let SearchOutput::JsonLines(jsonl) = outcome.output else {
                anyhow::bail!("{id}: expected JSON lines, got {:?}", outcome.output);
            };
            let oracle = std::fs::read(oracles.join(format!("{id}.jsonl")))?;
            assert_eq!(canonical_digest(&jsonl), canonical_digest(&oracle), "{id}");
        }
        Ok(())
    }

    #[test]
    fn library_msgpack_search_reproduces_the_cpp_tuples() -> anyhow::Result<()> {
        let root = prototype_root();
        let reader = RustLibraryReader::new(JsonOptions::default())?;
        let work = tempfile::tempdir()?;
        let options = SearchOptions::default();
        for (id, fixture, kql) in [
            ("wire-msgpack-search", "search.ts", "level: WARN"),
            ("wire-msgpack-nolog", "search.ts-nolog", "b: true"),
        ] {
            let archives = [root.join(format!("tests/fixtures/archives/{fixture}.clps"))];
            let outcome = reader.search(
                &SearchRequest {
                    archives: &archives,
                    kql,
                    options: &options,
                    output: OutputRequest::Msgpack,
                },
                work.path(),
            )?;
            assert_eq!(outcome.failure, None, "{id}");
            let SearchOutput::Msgpack(mut tuples) = outcome.output else {
                anyhow::bail!("{id}: expected msgpack tuples, got {:?}", outcome.output);
            };
            let mut expected = WireTuple::decode_stream(&std::fs::read(
                root.join(format!("tests/queries/oracles/msgpack/{id}.msgpack")),
            )?)?;
            assert!(!expected.is_empty(), "{id}");
            tuples.sort();
            expected.sort();
            assert_eq!(tuples, expected, "{id}");
        }
        Ok(())
    }

    #[test]
    fn library_search_reports_failures_and_unsupported_outputs() -> anyhow::Result<()> {
        let archives = [prototype_root().join("tests/fixtures/archives/search.ts.clps")];
        let reader = RustLibraryReader::new(JsonOptions::default())?;
        let work = tempfile::tempdir()?;
        let options = SearchOptions::default();
        let request = |kql, output| SearchRequest {
            archives: &archives,
            kql,
            options: &options,
            output,
        };
        let invalid = reader.search(
            &request("a: \"unterminated", OutputRequest::Stdout),
            work.path(),
        )?;
        assert_eq!(
            invalid.failure.and_then(|failure| failure.kind),
            Some(ErrorKind::InvalidQuery)
        );
        assert_eq!(invalid.output, SearchOutput::JsonLines(Vec::new()));
        let results_cache = reader.search(
            &request(
                "*",
                OutputRequest::ResultsCache {
                    max_num_results: 10,
                    dataset: None,
                },
            ),
            work.path(),
        )?;
        assert_eq!(results_cache.output, SearchOutput::Unsupported);
        let missing = [work.path().join("missing.clps")];
        let outcome = reader.search(
            &SearchRequest {
                archives: &missing,
                ..request("*", OutputRequest::Stdout)
            },
            work.path(),
        )?;
        assert_eq!(
            outcome.failure.and_then(|failure| failure.kind),
            Some(ErrorKind::Io)
        );
        Ok(())
    }
}
