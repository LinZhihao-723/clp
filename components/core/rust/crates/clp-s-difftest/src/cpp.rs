//! The C++ `clp-s` runner, and the C++ binary as a [`ReaderUnderTest`] (the harness self-test).

use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

use crate::reader::ExtractOutcome;
use crate::reader::OutputRequest;
use crate::reader::ReaderFailure;
use crate::reader::ReaderUnderTest;
use crate::reader::SearchOptions;
use crate::reader::SearchOutcome;
use crate::reader::SearchOutput;
use crate::reader::SearchRequest;
use crate::reader::WireTuple;

/// The C++ binary built in this repository (`build/core/clp-s`), from this branch.
pub const REPOSITORY_CPP_CLP_S: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../../build/core/clp-s"
);

/// Runs the C++ `clp-s` binary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CppClpS {
    pub binary: PathBuf,
}

impl CppClpS {
    /// Runs `clp-s x <archive> <output_dir>`.
    ///
    /// # Returns
    ///
    /// The decompressed JSON lines (`output_dir/original`) on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if the process exits with a non-zero status.
    /// * Forwards [`Self::extract_status`]'s return values on failure.
    pub fn extract(&self, archive: &Path, output_dir: &Path) -> anyhow::Result<Vec<u8>> {
        let run = self.extract_status(archive, output_dir)?;
        if run.exit_code != 0 {
            anyhow::bail!(
                "clp-s x {} exited with {}: {}",
                archive.display(),
                run.exit_code,
                run.stderr
            );
        }
        Ok(run.output)
    }

    /// Runs `clp-s x <archive> <output_dir>` and keeps a failing exit status.
    ///
    /// # Returns
    ///
    /// The exit code, the concatenation of every file the run wrote (in name order) and the
    /// standard error on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if `output_dir` cannot be created or read.
    /// * Forwards [`run_command`]'s return values on failure.
    pub fn extract_status(&self, archive: &Path, output_dir: &Path) -> anyhow::Result<CppRun> {
        std::fs::create_dir_all(output_dir)?;
        let mut command = Command::new(&self.binary);
        command.arg("x").arg(archive).arg(output_dir);
        let output = run_command(&mut command)?;
        let mut files: Vec<PathBuf> = std::fs::read_dir(output_dir)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<_, _>>()?;
        files.sort();
        let mut data = Vec::new();
        for file in files {
            if file.is_file() {
                data.extend(std::fs::read(&file)?);
            }
        }
        Ok(CppRun::new(&output, data))
    }

    /// Runs `clp-s s` on one archive, or on a directory of archives, with the given output
    /// handler. The arguments follow `scripts/corpus.py` (`run_search`): options, then the output
    /// handler, then `--projection`.
    ///
    /// # Returns
    ///
    /// The exit code, the output (stdout, or the contents of the output file) and the standard
    /// error on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if the output file exists but cannot be read.
    /// * Forwards [`run_command`]'s return values on failure.
    pub fn search(
        &self,
        archive: &Path,
        kql: &str,
        options: &SearchOptions,
        output: &CppOutput,
    ) -> anyhow::Result<CppRun> {
        let mut command = Command::new(&self.binary);
        command.arg("s").arg(archive).arg(kql);
        if options.is_case_insensitive {
            command.arg("-i");
        }
        if let Some(begin) = options.time_begin_ms {
            command.arg("--tge").arg(begin.to_string());
        }
        if let Some(end) = options.time_end_ms {
            command.arg("--tle").arg(end.to_string());
        }
        if let CppOutput::File { path } = output {
            command.arg("file").arg("--path").arg(path);
        }
        if !options.projection.is_empty() {
            command.arg("--projection").args(&options.projection);
        }
        let process_output = run_command(&mut command)?;
        let data = match output {
            CppOutput::Stdout => process_output.stdout.clone(),
            CppOutput::File { path } if path.exists() => std::fs::read(path)?,
            CppOutput::File { .. } => Vec::new(),
        };
        Ok(CppRun::new(&process_output, data))
    }

    /// Runs `clp-s c --single-file-archive <flags> <output_dir> <input>`.
    ///
    /// # Returns
    ///
    /// The paths of the archives written into `output_dir`, sorted, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if the process exits with a non-zero status, or `output_dir` cannot be
    ///   created or read.
    /// * Forwards [`run_command`]'s return values on failure.
    pub fn compress(
        &self,
        input: &Path,
        output_dir: &Path,
        flags: &[String],
    ) -> anyhow::Result<Vec<PathBuf>> {
        std::fs::create_dir_all(output_dir)?;
        let mut command = Command::new(&self.binary);
        command
            .arg("c")
            .arg("--single-file-archive")
            .args(flags)
            .arg(output_dir)
            .arg(input);
        let output = run_command(&mut command)?;
        let run = CppRun::new(&output, Vec::new());
        if run.exit_code != 0 {
            anyhow::bail!(
                "clp-s c {} exited with {}: {}",
                input.display(),
                run.exit_code,
                run.stderr
            );
        }
        let mut archives: Vec<PathBuf> = std::fs::read_dir(output_dir)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<_, _>>()?;
        archives.sort();
        Ok(archives)
    }
}

impl Default for CppClpS {
    fn default() -> Self {
        Self {
            binary: default_cpp_clp_s(),
        }
    }
}

/// A C++ output handler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CppOutput {
    Stdout,

    /// `file --path <path>` (msgpack tuples).
    File {
        path: PathBuf,
    },
}

/// The result of one C++ run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CppRun {
    /// The exit code; `128 + signal` if a signal killed the process.
    pub exit_code: i32,

    pub output: Vec<u8>,
    pub stderr: String,
}

impl CppRun {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The run of `process_output` with `output` as its output.
    fn new(process_output: &Output, output: Vec<u8>) -> Self {
        Self {
            exit_code: exit_code(process_output.status),
            output,
            stderr: String::from_utf8_lossy(&process_output.stderr)
                .trim()
                .to_owned(),
        }
    }

    /// # Returns
    ///
    /// The reader failure of a non-zero exit, if any.
    fn failure(&self) -> Option<ReaderFailure> {
        (self.exit_code != 0).then(|| ReaderFailure {
            kind: None,
            exit_code: Some(self.exit_code),
            message: self.stderr.clone(),
        })
    }
}

/// The C++ `clp-s` as the reader under test: the self-test of the harness, which must reproduce
/// every C++ oracle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CppReader {
    clp_s: CppClpS,
}

impl CppReader {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A reader that runs `clp_s`.
    #[must_use]
    pub const fn new(clp_s: CppClpS) -> Self {
        Self { clp_s }
    }
}

impl ReaderUnderTest for CppReader {
    fn name(&self) -> &'static str {
        "cpp"
    }

    fn extract(&self, archive: &Path, work_dir: &Path) -> anyhow::Result<ExtractOutcome> {
        let run = self.clp_s.extract_status(archive, &work_dir.join("x"))?;
        Ok(ExtractOutcome {
            failure: run.failure(),
            jsonl: run.output,
        })
    }

    fn search(
        &self,
        request: &SearchRequest<'_>,
        work_dir: &Path,
    ) -> anyhow::Result<SearchOutcome> {
        let archive = session_path(request.archives, work_dir)?;
        let (output, path) = match &request.output {
            OutputRequest::Stdout => (CppOutput::Stdout, None),
            OutputRequest::Msgpack => {
                let path = work_dir.join("results.msgpack");
                (CppOutput::File { path: path.clone() }, Some(path))
            }
            OutputRequest::ResultsCache { .. } => {
                return Ok(SearchOutcome {
                    output: SearchOutput::Unsupported,
                    failure: None,
                });
            }
        };
        let run = self
            .clp_s
            .search(&archive, request.kql, request.options, &output)?;
        let failure = run.failure();
        let output = if path.is_some() {
            SearchOutput::Msgpack(WireTuple::decode_stream(&run.output)?)
        } else {
            SearchOutput::JsonLines(run.output)
        };
        Ok(SearchOutcome { output, failure })
    }
}

/// # Returns
///
/// The default C++ binary: `$CLP_S`, else [`REPOSITORY_CPP_CLP_S`].
#[must_use]
pub fn default_cpp_clp_s() -> PathBuf {
    std::env::var_os("CLP_S").map_or_else(|| PathBuf::from(REPOSITORY_CPP_CLP_S), PathBuf::from)
}

/// The archive argument of a session: the archive itself, or a directory of symlinks to the
/// archives (named by their file names, as `scripts/corpus.py` does).
///
/// # Returns
///
/// The path to pass to `clp-s` on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if there is no archive, an archive has no file name, or a symlink cannot be
///   created.
pub fn session_path(archives: &[PathBuf], work_dir: &Path) -> anyhow::Result<PathBuf> {
    match archives {
        [] => anyhow::bail!("a session needs at least one archive"),
        [archive] => Ok(archive.clone()),
        _ => {
            let directory = work_dir.join("archives");
            std::fs::create_dir_all(&directory)?;
            for archive in archives {
                let name = archive.file_name().ok_or_else(|| {
                    anyhow::anyhow!("archive path {} has no file name", archive.display())
                })?;
                std::os::unix::fs::symlink(archive, directory.join(name))?;
            }
            Ok(directory)
        }
    }
}

/// Runs a command to completion, capturing its output.
///
/// # Returns
///
/// The process output on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if the process cannot be spawned.
pub fn run_command(command: &mut Command) -> anyhow::Result<Output> {
    command
        .output()
        .map_err(|error| anyhow::anyhow!("cannot run {}: {error}", command.get_program().display()))
}

/// # Returns
///
/// The exit code of `status`, or `128 + signal` if a signal ended the process.
#[must_use]
pub fn exit_code(status: std::process::ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::CppClpS;
    use super::CppOutput;
    use super::exit_code;
    use super::session_path;
    use crate::canonical::canonical_digest;
    use crate::corpus::prototype_root;
    use crate::reader::SearchOptions;
    use crate::reader::WireTuple;

    /// # Returns
    ///
    /// The path of a fixture archive.
    fn fixture_archive(name: &str) -> PathBuf {
        prototype_root().join("tests/fixtures/archives").join(name)
    }

    #[test]
    fn extract_reproduces_the_fixture_oracle() -> anyhow::Result<()> {
        let work = tempfile::tempdir()?;
        let output =
            CppClpS::default().extract(&fixture_archive("scalars.default.clps"), work.path())?;
        let oracle =
            std::fs::read(prototype_root().join("tests/fixtures/oracles/scalars.default.jsonl"))?;
        assert_eq!(canonical_digest(&output), canonical_digest(&oracle));
        Ok(())
    }

    #[test]
    fn extract_reports_a_failing_exit() -> anyhow::Result<()> {
        let work = tempfile::tempdir()?;
        let missing = work.path().join("missing.clps");
        let cpp = CppClpS::default();
        assert_ne!(
            cpp.extract_status(&missing, &work.path().join("x"))?
                .exit_code,
            0
        );
        assert!(cpp.extract(&missing, &work.path().join("y")).is_err());
        Ok(())
    }

    #[test]
    fn search_passes_options_and_reads_the_output_file() -> anyhow::Result<()> {
        let work = tempfile::tempdir()?;
        let cpp = CppClpS::default();
        let archive = fixture_archive("search.ts.clps");
        let options = SearchOptions {
            is_case_insensitive: true,
            ..SearchOptions::default()
        };
        let stdout = cpp.search(&archive, "level: warn", &options, &CppOutput::Stdout)?;
        assert_eq!(stdout.exit_code, 0);
        assert_eq!(canonical_digest(&stdout.output).count, 6);
        let path = work.path().join("out.msgpack");
        let file = cpp.search(
            &archive,
            "level: WARN",
            &SearchOptions::default(),
            &CppOutput::File { path },
        )?;
        assert_eq!(WireTuple::decode_stream(&file.output)?.len(), 6);
        let bounded = SearchOptions {
            time_begin_ms: Some(1_700_000_010_000),
            time_end_ms: Some(1_700_000_010_000),
            projection: vec!["rid".to_owned()],
            ..SearchOptions::default()
        };
        let projected = cpp.search(&archive, "*", &bounded, &CppOutput::Stdout)?;
        assert_eq!(String::from_utf8(projected.output)?, "{\"rid\":\"r10\"}\n");
        Ok(())
    }

    #[test]
    fn compress_writes_a_single_file_archive() -> anyhow::Result<()> {
        let work = tempfile::tempdir()?;
        let input = work.path().join("input.jsonl");
        std::fs::write(&input, "{\"a\":1,\"b\":\"x y\"}\n{\"a\":2}\n")?;
        let cpp = CppClpS::default();
        let archives = cpp.compress(&input, &work.path().join("archives"), &[])?;
        assert_eq!(archives.len(), 1);
        let output = cpp.extract(&archives[0], &work.path().join("x"))?;
        assert_eq!(
            canonical_digest(&output),
            canonical_digest(&std::fs::read(&input)?)
        );
        Ok(())
    }

    #[test]
    fn session_path_links_several_archives_into_a_directory() -> anyhow::Result<()> {
        let work = tempfile::tempdir()?;
        let first = fixture_archive("logs.ts-split.0.clps");
        let second = fixture_archive("logs.ts-split.1.clps");
        assert_eq!(
            session_path(std::slice::from_ref(&first), work.path())?,
            first
        );
        let directory = session_path(&[first.clone(), second], work.path())?;
        assert_eq!(
            std::fs::read_link(directory.join("logs.ts-split.0.clps"))?,
            first
        );
        assert!(session_path(&[], work.path()).is_err());
        Ok(())
    }

    #[test]
    fn signals_map_to_128_plus_the_signal() -> anyhow::Result<()> {
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg("kill -9 $$")
            .status()?;
        assert_eq!(exit_code(status), 137);
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 3")
            .status()?;
        assert_eq!(exit_code(status), 3);
        Ok(())
    }
}
