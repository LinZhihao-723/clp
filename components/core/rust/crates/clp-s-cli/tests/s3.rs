//! `clp-s-rs --auth s3` as a process. Against the in-process mock S3 server: every fixture
//! archive extracts, and every fixture query of the suite searches, exactly like the local files;
//! local and S3 inputs form one session; bad S3 inputs map to exit codes. Against a real S3 server
//! (ignored by default, `crates/clp-s-object-store/scripts/run-s3-tests.sh`): the fixtures, the
//! dataset archives and the suite queries read through S3 equal the local results and the C++
//! `clp-s --auth s3` results from the same endpoint.

#![cfg(feature = "s3")]

use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

use anyhow::Context;
use clp_s_difftest::compare::RecordComparison;
use clp_s_difftest::corpus::Corpus;
use clp_s_difftest::corpus::FixtureKind;
use clp_s_difftest::corpus::OutputKind;
use clp_s_difftest::corpus::QueryCase;
use clp_s_difftest::corpus::QueryTarget;
use clp_s_difftest::corpus::prototype_root;
use clp_s_difftest::cpp::default_cpp_clp_s;
use clp_s_difftest::multiset::RecordMultiset;
use clp_s_object_store::test_util::MockS3Server;

/// The `clp-s-rs` binary under test.
const BINARY: &str = env!("CARGO_BIN_EXE_clp-s-rs");

/// The environment variable naming the real S3 server's endpoint, e.g. `http://127.0.0.1:18333`.
const ENDPOINT_VARIABLE: &str = "CLP_S_TEST_S3_ENDPOINT";

const BUCKET: &str = "proto-archives";

/// The credentials a child process gets; the rest of its AWS environment is cleared, so the
/// developer's profiles, region and endpoint never leak into a test.
struct Credentials {
    access_key_id: String,
    secret_access_key: String,
}

impl Credentials {
    /// # Returns
    ///
    /// The credentials the mock server accepts (it accepts any).
    fn mock() -> Self {
        Self {
            access_key_id: "test".to_owned(),
            secret_access_key: "test".to_owned(),
        }
    }

    /// # Returns
    ///
    /// The real server's credentials, from `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`.
    ///
    /// # Panics
    ///
    /// Panics if either is unset.
    fn from_env() -> Self {
        Self {
            access_key_id: std::env::var("AWS_ACCESS_KEY_ID")
                .expect("AWS_ACCESS_KEY_ID names the test server's access key"),
            secret_access_key: std::env::var("AWS_SECRET_ACCESS_KEY")
                .expect("AWS_SECRET_ACCESS_KEY names the test server's secret key"),
        }
    }

    /// # Type Parameters
    ///
    /// * `ArgType` - The type of a command-line argument.
    ///
    /// # Returns
    ///
    /// The output of `program` with `args`, run with only these credentials in its AWS
    /// environment, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Command::output`]'s return values on failure.
    fn run<ArgType: AsRef<OsStr>>(
        &self,
        program: &str,
        args: &[ArgType],
    ) -> anyhow::Result<Output> {
        let mut command = Command::new(program);
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("AWS_") {
                command.env_remove(name);
            }
        }
        command
            .args(args)
            .env("AWS_ACCESS_KEY_ID", &self.access_key_id)
            .env("AWS_SECRET_ACCESS_KEY", &self.secret_access_key)
            .env("AWS_CONFIG_FILE", "/dev/null")
            .env("AWS_SHARED_CREDENTIALS_FILE", "/dev/null")
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .output()
            .with_context(|| format!("`{program}` cannot be spawned"))
    }
}

/// A multi-thread runtime and a mock S3 server holding every supported fixture archive under its
/// file name.
struct MockFixture {
    server: MockS3Server,
    _runtime: tokio::runtime::Runtime,
}

impl MockFixture {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The fixture on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`tokio::runtime::Builder::build`]'s return values on failure.
    /// * Forwards [`MockS3Server::start`]'s return values on failure.
    /// * Forwards [`std::fs::read`]'s return values on failure.
    fn new(archives: &[PathBuf]) -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let server = MockS3Server::start(runtime.handle())?;
        for archive in archives {
            server.put_object(BUCKET, &file_name(archive), std::fs::read(archive)?);
        }
        Ok(Self {
            server,
            _runtime: runtime,
        })
    }

    /// # Returns
    ///
    /// The path-style URL of the object holding `archive`.
    fn url(&self, archive: &Path) -> String {
        format!(
            "{}/{BUCKET}/{}",
            self.server.endpoint_url(),
            file_name(archive)
        )
    }
}

/// # Returns
///
/// The fixture archives the reader supports, sorted.
fn supported_fixture_archives(corpus: &Corpus) -> Vec<PathBuf> {
    let mut archives: Vec<PathBuf> = corpus
        .archive_oracles
        .iter()
        .filter(|oracle| matches!(oracle.kind, FixtureKind::Sfa | FixtureKind::Kvir))
        .map(|oracle| oracle.archive.clone())
        .collect();
    archives.sort();
    archives.dedup();
    archives
}

/// # Returns
///
/// The file name of `path`.
///
/// # Panics
///
/// Panics if `path` doesn't name a file.
fn file_name(path: &Path) -> String {
    path.file_name()
        .expect("archive paths name a file")
        .to_string_lossy()
        .into_owned()
}

/// # Returns
///
/// The archives `case` searches: every archive of its fixture, or its dataset archive.
fn case_archives(corpus: &Corpus, case: &QueryCase) -> Vec<PathBuf> {
    match &case.target {
        QueryTarget::Fixture(name) => corpus
            .fixture(name)
            .expect("the case's fixture exists")
            .archives
            .clone(),
        QueryTarget::Dataset(name) => vec![
            corpus
                .dataset(name)
                .expect("the case's dataset exists")
                .archive
                .clone(),
        ],
    }
}

/// # Returns
///
/// The `clp-s-rs search` arguments of `case` after the inputs.
fn rust_search_args(case: &QueryCase) -> Vec<String> {
    let mut args = vec![format!("--query={}", case.kql)];
    if case.options.is_case_insensitive {
        args.push("-i".to_owned());
    }
    if let Some(begin) = case.options.time_begin_ms {
        args.push(format!("--tge={begin}"));
    }
    if let Some(end) = case.options.time_end_ms {
        args.push(format!("--tle={end}"));
    }
    if !case.options.projection.is_empty() {
        args.push("--projection".to_owned());
        args.extend(case.options.projection.iter().cloned());
    }
    args
}

/// Runs `clp-s-rs extract` over `inputs` with `extra_args` into a fresh directory.
///
/// # Type Parameters
///
/// * `InputType` - The type of an input path or URL.
///
/// # Returns
///
/// The extracted JSON lines on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if the process exits with a non-zero status.
/// * Forwards [`tempfile::tempdir`]'s return values on failure.
/// * Forwards [`Credentials::run`]'s return values on failure.
fn rust_extract<InputType: AsRef<OsStr>>(
    credentials: &Credentials,
    inputs: &[InputType],
    extra_args: &[&str],
) -> anyhow::Result<Vec<u8>> {
    let output_dir = tempfile::tempdir()?;
    let mut args: Vec<OsString> = vec!["x".into()];
    args.extend(inputs.iter().map(|input| input.as_ref().to_owned()));
    args.extend(extra_args.iter().map(OsString::from));
    args.push("-o".into());
    args.push(output_dir.path().into());
    let output = credentials.run(BINARY, &args)?;
    anyhow::ensure!(
        output.status.success(),
        "clp-s-rs x failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(std::fs::read(output_dir.path().join("original")).unwrap_or_default())
}

/// # Type Parameters
///
/// * `InputType` - The type of an input path or URL.
///
/// # Returns
///
/// The output of `clp-s-rs search <inputs> <args>` on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`Credentials::run`]'s return values on failure.
fn rust_search<InputType: AsRef<OsStr>>(
    credentials: &Credentials,
    inputs: &[InputType],
    args: &[String],
) -> anyhow::Result<Output> {
    let mut command_args: Vec<OsString> = vec!["s".into()];
    command_args.extend(inputs.iter().map(|input| input.as_ref().to_owned()));
    command_args.extend(args.iter().map(OsString::from));
    credentials.run(BINARY, &command_args)
}

#[test]
fn fixture_archives_extract_through_s3_like_local_files() -> anyhow::Result<()> {
    let corpus = Corpus::load(&prototype_root())?;
    let archives = supported_fixture_archives(&corpus);
    assert!(archives.len() > 40, "{archives:?}");
    let fixture = MockFixture::new(&archives)?;
    let credentials = Credentials::mock();
    let local = rust_extract(&credentials, &archives, &[])?;
    assert!(!local.is_empty());

    let urls: Vec<String> = archives
        .iter()
        .map(|archive| fixture.url(archive))
        .collect();
    assert_eq!(
        rust_extract(&credentials, &urls, &["--auth", "s3"])?,
        local,
        "path-style HTTP URLs"
    );

    let s3_urls: Vec<String> = archives
        .iter()
        .map(|archive| format!("s3://{BUCKET}/{}", file_name(archive)))
        .collect();
    assert_eq!(
        rust_extract(
            &credentials,
            &s3_urls,
            &[
                "--auth",
                "s3",
                "--s3-endpoint-url",
                fixture.server.endpoint_url(),
                "--s3-region",
                "eu-west-1"
            ]
        )?,
        local,
        "s3:// URLs with an endpoint override"
    );
    Ok(())
}

#[test]
fn fixture_queries_search_through_s3_like_local_files() -> anyhow::Result<()> {
    let corpus = Corpus::load(&prototype_root())?;
    let fixture = MockFixture::new(&supported_fixture_archives(&corpus))?;
    let credentials = Credentials::mock();
    let mut num_cases = 0;
    for case in &corpus.queries {
        if !matches!(case.target, QueryTarget::Fixture(_))
            || !case.outputs.contains(&OutputKind::Stdout)
        {
            continue;
        }
        let archives = case_archives(&corpus, case);
        if archives.iter().any(|archive| !archive.is_file()) {
            continue;
        }
        let urls: Vec<String> = archives
            .iter()
            .map(|archive| fixture.url(archive))
            .collect();
        let args = rust_search_args(case);
        let local = rust_search(&credentials, &archives, &args)?;
        let mut s3_args = args.clone();
        s3_args.extend(["--auth".to_owned(), "s3".to_owned()]);
        let remote = rust_search(&credentials, &urls, &s3_args)?;
        assert_eq!(
            (remote.status.code(), &remote.stdout),
            (local.status.code(), &local.stdout),
            "{}: {}",
            case.id,
            String::from_utf8_lossy(&remote.stderr)
        );
        num_cases += 1;
    }
    assert!(num_cases > 200, "{num_cases}");
    Ok(())
}

#[test]
fn local_and_s3_inputs_form_one_session() -> anyhow::Result<()> {
    let corpus = Corpus::load(&prototype_root())?;
    let archives = corpus
        .fixture("logs.ts-split")
        .expect("the split fixture exists")
        .archives
        .clone();
    assert!(archives.len() > 2, "{archives:?}");
    let fixture = MockFixture::new(&archives)?;
    let credentials = Credentials::mock();
    let args = [
        "--query=*".to_owned(),
        "--max-num-results=5".to_owned(),
        "--auth=s3".to_owned(),
    ];
    let local = rust_search(&credentials, &archives, &args)?;
    let mixed: Vec<OsString> = archives
        .iter()
        .enumerate()
        .map(|(index, archive)| {
            if 0 == index % 2 {
                archive.clone().into_os_string()
            } else {
                fixture.url(archive).into()
            }
        })
        .collect();
    let remote = rust_search(&credentials, &mixed, &args)?;
    assert_eq!(local.status.code(), Some(0));
    assert_eq!(local.stdout.split(|byte| b'\n' == *byte).count(), 6);
    assert_eq!(
        (remote.status.code(), remote.stdout),
        (local.status.code(), local.stdout),
        "{}",
        String::from_utf8_lossy(&remote.stderr)
    );
    Ok(())
}

#[test]
fn bad_s3_inputs_map_to_exit_codes() -> anyhow::Result<()> {
    let corpus = Corpus::load(&prototype_root())?;
    let archives = supported_fixture_archives(&corpus);
    let fixture = MockFixture::new(&archives[..1])?;
    let credentials = Credentials::mock();
    let endpoint = fixture.server.endpoint_url().to_owned();
    for (input, expected_exit, expected_message) in [
        (
            format!("{endpoint}/{BUCKET}/missing.clps"),
            3,
            "cannot stat",
        ),
        ("s3://bucket".to_owned(), 1, "invalid S3 URL"),
        ("https://Host/bucket/key".to_owned(), 1, "invalid S3 URL"),
        (
            format!("{endpoint}/{BUCKET}/dir/"),
            1,
            "cannot derive an archive id",
        ),
        (
            format!("s3://{BUCKET}/dir/"),
            1,
            "cannot derive an archive id",
        ),
    ] {
        let output = rust_search(
            &credentials,
            &[input.as_str()],
            &[
                "--query=*".to_owned(),
                "--auth=s3".to_owned(),
                format!("--s3-endpoint-url={endpoint}"),
            ],
        )?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(expected_exit),
            "{input}: {stderr}"
        );
        assert!(stderr.contains(expected_message), "{input}: {stderr}");
        assert_eq!(output.stdout, b"", "{input}");
    }
    Ok(())
}

/// A real S3 server named by [`ENDPOINT_VARIABLE`], holding the fixture and dataset archives.
struct ServerFixture {
    endpoint: String,
    credentials: Credentials,
}

impl ServerFixture {
    /// Factory function.
    ///
    /// Creates [`BUCKET`] (if missing) and uploads `archives` under their file names.
    ///
    /// # Returns
    ///
    /// The fixture on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`std::env::var`]'s return values on failure.
    /// * Forwards [`tokio::runtime::Builder::build`]'s return values on failure.
    /// * Forwards [`aws_sdk_s3::Client::create_bucket`]'s return values on failure.
    /// * Forwards [`ByteStream::from_path`]'s return values on failure.
    /// * Forwards [`aws_sdk_s3::Client::put_object`]'s return values on failure.
    ///
    /// # Panics
    ///
    /// Panics if the credentials are unset.
    fn new(archives: &[PathBuf]) -> anyhow::Result<Self> {
        use aws_sdk_s3::config::BehaviorVersion;
        use aws_sdk_s3::config::Region;
        use aws_sdk_s3::primitives::ByteStream;

        let endpoint = std::env::var(ENDPOINT_VARIABLE)
            .with_context(|| format!("{ENDPOINT_VARIABLE} names the test server"))?;
        let credentials = Credentials::from_env();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .endpoint_url(&endpoint)
            .force_path_style(true)
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                &credentials.access_key_id,
                &credentials.secret_access_key,
                None,
                None,
                "test",
            ))
            .build();
        let client = aws_sdk_s3::Client::from_conf(config);
        runtime.block_on(async {
            if client.head_bucket().bucket(BUCKET).send().await.is_err() {
                client
                    .create_bucket()
                    .bucket(BUCKET)
                    .send()
                    .await
                    .context("the test bucket cannot be created")?;
            }
            for archive in archives {
                client
                    .put_object()
                    .bucket(BUCKET)
                    .key(file_name(archive))
                    .body(ByteStream::from_path(archive).await?)
                    .send()
                    .await
                    .with_context(|| format!("{} cannot be uploaded", archive.display()))?;
            }
            anyhow::Ok(())
        })?;
        Ok(Self {
            endpoint,
            credentials,
        })
    }

    /// # Returns
    ///
    /// The path-style URL of the object holding `archive`.
    fn url(&self, archive: &Path) -> String {
        format!("{}/{BUCKET}/{}", self.endpoint, file_name(archive))
    }

    /// # Returns
    ///
    /// The output of the C++ `clp-s` with `args` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Credentials::run`]'s return values on failure.
    fn cpp(&self, args: &[String]) -> anyhow::Result<Output> {
        self.credentials
            .run(&default_cpp_clp_s().to_string_lossy(), args)
    }

    /// Runs C++ `clp-s x --auth s3 <url> <dir>`.
    ///
    /// # Returns
    ///
    /// The extracted JSON lines on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if the process exits with a non-zero status.
    /// * Forwards [`tempfile::tempdir`]'s return values on failure.
    /// * Forwards [`Self::cpp`]'s return values on failure.
    /// * Forwards [`std::fs::read_dir`]'s return values on failure.
    /// * Forwards [`std::fs::read`]'s return values on failure.
    fn cpp_extract(&self, input: &str) -> anyhow::Result<Vec<u8>> {
        let output_dir = tempfile::tempdir()?;
        let output = self.cpp(&[
            "x".to_owned(),
            "--auth".to_owned(),
            "s3".to_owned(),
            input.to_owned(),
            output_dir.path().to_string_lossy().into_owned(),
        ])?;
        anyhow::ensure!(
            output.status.success(),
            "clp-s x {input} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let mut files = std::fs::read_dir(output_dir.path())?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<PathBuf>, _>>()?;
        files.sort();
        let mut records = Vec::new();
        for file in &files {
            records.extend(std::fs::read(file)?);
        }
        Ok(records)
    }

    /// # Returns
    ///
    /// The output of C++ `clp-s s --auth s3 <url> <case>`, with the arguments in the order of
    /// `clp_s_difftest::cpp::CppClpS::search`, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::cpp`]'s return values on failure.
    fn cpp_search(&self, url: &str, case: &QueryCase) -> anyhow::Result<Output> {
        let mut args = vec![
            "s".to_owned(),
            "--auth".to_owned(),
            "s3".to_owned(),
            url.to_owned(),
            case.kql.clone(),
        ];
        if case.options.is_case_insensitive {
            args.push("-i".to_owned());
        }
        if let Some(begin) = case.options.time_begin_ms {
            args.extend(["--tge".to_owned(), begin.to_string()]);
        }
        if let Some(end) = case.options.time_end_ms {
            args.extend(["--tle".to_owned(), end.to_string()]);
        }
        if !case.options.projection.is_empty() {
            args.push("--projection".to_owned());
            args.extend(case.options.projection.iter().cloned());
        }
        self.cpp(&args)
    }
}

/// Asserts that `rust` equals `cpp` up to the registered float divergences (DV-01, DV-02).
fn assert_matches_cpp(cpp: &[u8], rust: &[u8], what: &str) {
    let comparison = RecordComparison::compare(
        &RecordMultiset::from_jsonl(cpp),
        &RecordMultiset::from_jsonl(rust),
    );
    assert!(
        comparison.unexplained.is_empty(),
        "{what}: {}",
        comparison.render(5)
    );
}

#[test]
#[ignore = "needs an S3 server (crates/clp-s-object-store/scripts/run-s3-tests.sh)"]
fn s3_server_archives_read_like_local_files_and_like_cpp() -> anyhow::Result<()> {
    let corpus = Corpus::load(&prototype_root())?;
    let fixture_archives = supported_fixture_archives(&corpus);
    let dataset_archives: Vec<PathBuf> = corpus
        .datasets
        .iter()
        .map(|dataset| dataset.archive.clone())
        .filter(|archive| archive.is_file())
        .collect();
    assert_eq!(dataset_archives.len(), 3, "{dataset_archives:?}");
    let all_archives: Vec<PathBuf> = fixture_archives
        .iter()
        .chain(&dataset_archives)
        .cloned()
        .collect();
    let server = ServerFixture::new(&all_archives)?;
    let s3_args = [
        "--auth".to_owned(),
        "s3".to_owned(),
        format!("--s3-endpoint-url={}", server.endpoint),
    ];
    let s3_arg_refs: Vec<&str> = s3_args.iter().map(String::as_str).collect();

    for archive in &all_archives {
        let url = server.url(archive);
        let local = rust_extract(&server.credentials, &[archive], &[])?;
        let remote = rust_extract(
            &server.credentials,
            &[format!("s3://{BUCKET}/{}", file_name(archive))],
            &s3_arg_refs,
        )?;
        assert!(
            remote == local,
            "x {url}: the S3 output differs from the local output"
        );
        assert_matches_cpp(&server.cpp_extract(&url)?, &remote, &format!("x {url}"));
    }

    let mut num_cpp_cases = 0;
    let mut num_fixture_cases = 0;
    for case in &corpus.queries {
        let archives = case_archives(&corpus, case);
        // C++ searches one URL per invocation.
        if !case.outputs.contains(&OutputKind::Stdout) || archives.len() != 1 {
            continue;
        }
        // Every dataset query, and every third fixture query.
        if matches!(case.target, QueryTarget::Fixture(_)) {
            num_fixture_cases += 1;
            if 0 != num_fixture_cases % 3 {
                continue;
            }
        }
        let url = server.url(&archives[0]);
        let args = rust_search_args(case);
        let local = rust_search(&server.credentials, &archives, &args)?;
        let mut remote_args = args.clone();
        remote_args.extend(s3_args.iter().cloned());
        let remote = rust_search(&server.credentials, &[url.as_str()], &remote_args)?;
        assert!(
            remote.status.code() == local.status.code() && remote.stdout == local.stdout,
            "{}: the S3 search differs from the local search: {}",
            case.id,
            String::from_utf8_lossy(&remote.stderr)
        );
        if !case.divergences.is_empty() || remote.status.code() != Some(0) {
            continue;
        }
        let cpp = server.cpp_search(&url, case)?;
        assert_eq!(
            cpp.status.code(),
            Some(0),
            "{}: {}",
            case.id,
            String::from_utf8_lossy(&cpp.stderr)
        );
        assert_matches_cpp(&cpp.stdout, &remote.stdout, &case.id);
        num_cpp_cases += 1;
    }
    assert!(num_cpp_cases > 40, "{num_cpp_cases}");
    Ok(())
}
