//! The `clp-s` subprocess search: a mirror of `components/clp-tdl-package/src/task/query/search.rs`
//! and the helpers it uses from `components/clp-tdl-package/src/task/utils.rs`, kept local so the
//! adapter does not depend on the package. Must be kept in sync.

use std::ffi::OsString;
use std::io::Read;
use std::num::NonZeroU16;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

use anyhow::Context;
use aws_config::BehaviorVersion;
use aws_sdk_s3::config::ProvideCredentials;
use clp_rust_utils::aws::AWS_DEFAULT_REGION;
use clp_rust_utils::clp_config::AwsAuthentication;
use clp_rust_utils::clp_config::package::config::ArchiveOutputStorage;
use clp_rust_utils::clp_config::package::config::SpiderTaskExecutorConfig;
use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::job_config::SessionToken;
use clp_rust_utils::s3::generate_s3_url;
use clp_rust_utils::task_io::query::ClpSQueryOption;
use clp_rust_utils::task_io::query::OutputHandle;
use clp_rust_utils::task_io::query::QueryTaskIndex;
use clp_rust_utils::types::ArchiveId;
use non_empty_string::NonEmptyString;

use crate::AdapterResources;
use crate::search::ArchiveToSearch;
use crate::search::SearchTask;

/// Searches one archive by spawning the C++ `clp-s`, exactly as `clp-tdl-package`'s `search` task
/// does today.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`SearchTask::new`]'s return values on failure.
/// * Forwards [`AdapterResources::global`]'s return values on failure.
/// * Forwards [`SearchTask::run_clp_s_subprocess`]'s return values on failure.
pub fn search_with_clp_s_subprocess(
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
    task.run_clp_s_subprocess(AdapterResources::global()?)
}

/// Mirror of `clp-tdl-package`'s `ArchiveSelector`: how clp-s addresses the archive to search.
#[derive(Debug, PartialEq, Eq)]
pub enum ArchiveSelector {
    /// A local dataset archives directory plus the `--archive-id` selecting one archive in it.
    Directory {
        path: PathBuf,
        archive_id: ArchiveId,
    },

    /// The URL of an S3-hosted archive, read with `--auth s3`.
    ObjectUrl(String),
}

/// Mirror of `clp-tdl-package`'s `resolve_archive_input`: resolves how clp-s addresses the archive,
/// and the credential env vars it needs to read it.
///
/// # Returns
///
/// A tuple on success, containing:
///
/// * The archive selector.
/// * The credential env vars clp-s should run with, which are empty for filesystem-backed archive
///   output.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`generate_s3_url`]'s return values on failure.
/// * Forwards [`s3_credential_env`]'s return values on failure.
pub fn resolve_archive_input(
    runtime: &tokio::runtime::Handle,
    clp_home: &Path,
    config: &SpiderTaskExecutorConfig,
    dataset: &str,
    archive_id: ArchiveId,
) -> anyhow::Result<(ArchiveSelector, Vec<(&'static str, String)>)> {
    let s3_config = match &config.archive_output.storage {
        ArchiveOutputStorage::Fs { .. } => {
            return Ok((
                ArchiveSelector::Directory {
                    path: config.abs_archive_output_staging(clp_home).join(dataset),
                    archive_id,
                },
                Vec::new(),
            ));
        }
        ArchiveOutputStorage::S3 { s3_config, .. } => s3_config,
    };

    let object_key = config
        .archive_output
        .dataset_archive_object_key(Some(dataset), &archive_id);
    let url = generate_s3_url(
        s3_config.endpoint_url.as_ref().map(NonEmptyString::as_str),
        s3_config.region_code.as_ref().map(NonEmptyString::as_str),
        &s3_config.bucket,
        &object_key,
    )?;
    let region = s3_config
        .region_code
        .as_ref()
        .map_or(AWS_DEFAULT_REGION, NonEmptyString::as_str);
    let credential_env = s3_credential_env(runtime, region, &s3_config.aws_authentication)?;
    Ok((ArchiveSelector::ObjectUrl(url), credential_env))
}

/// Mirror of `clp-tdl-package`'s `build_clp_s_search_args_for_result_cache`: builds the clp-s
/// command-line arguments for a single-archive search writing to the result cache.
///
/// # Returns
///
/// The ordered clp-s arguments.
pub fn build_clp_s_search_args_for_result_cache(
    archive_selector: &ArchiveSelector,
    clp_s_query_option: &ClpSQueryOption,
    result_cache_uri: &str,
    query_job_id: QueryJobId,
    dataset: &str,
) -> Vec<OsString> {
    let mut args = build_clp_s_search_query_args(archive_selector, clp_s_query_option);
    args.extend([
        OsString::from("results-cache"),
        OsString::from("--uri"),
        OsString::from(result_cache_uri),
        OsString::from("--collection"),
        OsString::from(query_job_id.to_string()),
    ]);
    if let Some(max_num_results) = clp_s_query_option.max_num_results {
        args.push(OsString::from("--max-num-results"));
        args.push(OsString::from(max_num_results.to_string()));
    }
    args.extend([OsString::from("--dataset"), OsString::from(dataset)]);
    args
}

/// Mirror of `clp-tdl-package`'s `build_clp_s_search_args_for_network`: builds the clp-s
/// command-line arguments for a single-archive search streaming to a network destination.
///
/// # Returns
///
/// The ordered clp-s arguments.
pub fn build_clp_s_search_args_for_network(
    archive_selector: &ArchiveSelector,
    clp_s_query_option: &ClpSQueryOption,
    host: &str,
    port: NonZeroU16,
    session_token: SessionToken,
    task_index: QueryTaskIndex,
) -> Vec<OsString> {
    let mut args = build_clp_s_search_query_args(archive_selector, clp_s_query_option);
    args.extend([
        OsString::from("network"),
        OsString::from("--host"),
        OsString::from(host),
        OsString::from("--port"),
        OsString::from(port.to_string()),
        OsString::from("--session-token"),
        OsString::from(session_token.to_string()),
        OsString::from("--task-index"),
        OsString::from(task_index.to_string()),
    ]);
    args
}

/// Mirror of `clp-tdl-package`'s `run_clp_s_search`: runs clp-s with the given search arguments,
/// blocking until it exits.
///
/// # Observability
///
/// This method logs errors on failure before returning to the caller. `clp-s`' stderr is logged if
/// successfully captured.
///
/// # Errors
///
/// Returns an error if:
///
/// * clp-s exits with a non-zero status.
/// * Forwards [`Command::spawn`]'s return values on failure.
/// * Forwards [`std::process::Child::wait`]'s return values on failure.
///
/// # Panics
///
/// Panics if clp-s's piped stderr is unexpectedly absent (should be unreachable).
pub fn run_clp_s_search(
    clp_s_bin: &Path,
    args: Vec<OsString>,
    credential_env: &[(&'static str, String)],
) -> anyhow::Result<()> {
    let mut child = Command::new(clp_s_bin)
        .args(args)
        .envs(credential_env.iter().cloned())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn clp-s at {}", clp_s_bin.display()))
        .inspect_err(|e| {
            tracing::error!(
                error = % e,
                clp_s_bin = % clp_s_bin.display(),
                "Failed to spawn clp-s.",
            );
        })?;

    let mut stderr = child
        .stderr
        .take()
        .expect("piped stderr should always be present");

    let mut captured_stderr = String::new();
    if let Err(e) = stderr.read_to_string(&mut captured_stderr) {
        captured_stderr = format!("failed to read clp-s stderr: {e}");
    }

    let status = child
        .wait()
        .context("failed to wait for clp-s to exit")
        .inspect_err(|e| {
            tracing::error!(
                error = % e,
                stderr = % captured_stderr,
                "Failed to wait for clp-s to exit."
            );
        })?;
    if !status.success() {
        tracing::error!(
            status = status.code(),
            stderr = % captured_stderr,
            "clp-s exited on failure."
        );
        anyhow::bail!("clp-s exited on error with status={status}");
    }
    Ok(())
}

/// Mirror of `clp-tdl-package`'s `clp_binary_path`: resolves the path of a CLP binary under
/// `clp_home`, joining `bin/{binary}`.
///
/// # Returns
///
/// The path to the named binary under the CLP installation.
pub fn clp_binary_path(clp_home: &Path, binary: &str) -> PathBuf {
    clp_home.join("bin").join(binary)
}

/// Mirror of `clp-tdl-package`'s `build_clp_s_search_query_args`: builds the clp-s command-line
/// arguments that select the archive and the query to run on it, shared by every output handler.
///
/// # Returns
///
/// The ordered clp-s arguments, which the caller completes with an output handler.
fn build_clp_s_search_query_args(
    archive_selector: &ArchiveSelector,
    clp_s_query_option: &ClpSQueryOption,
) -> Vec<OsString> {
    let mut args = vec![OsString::from("s")];
    match archive_selector {
        ArchiveSelector::Directory { path, archive_id } => {
            args.push(path.as_os_str().to_os_string());
            args.push(OsString::from("--archive-id"));
            args.push(OsString::from(archive_id.to_string()));
        }
        ArchiveSelector::ObjectUrl(url) => {
            args.push(OsString::from(url));
            args.push(OsString::from("--auth"));
            args.push(OsString::from("s3"));
        }
    }

    args.push(OsString::from(clp_s_query_option.query_string.as_str()));
    if let Some(begin_timestamp_millisecs) = clp_s_query_option.begin_timestamp_millisecs {
        args.push(OsString::from("--tge"));
        args.push(OsString::from(begin_timestamp_millisecs.to_string()));
    }
    if let Some(end_timestamp_millisecs) = clp_s_query_option.end_timestamp_millisecs {
        args.push(OsString::from("--tle"));
        args.push(OsString::from(end_timestamp_millisecs.to_string()));
    }
    if clp_s_query_option.ignore_case {
        args.push(OsString::from("--ignore-case"));
    }
    args
}

/// Mirror of `clp-tdl-package`'s `s3_credential_env`: resolves the AWS credential env vars clp-s
/// needs to access the S3 objects.
///
/// # Returns
///
/// The env-var name-value pairs with the following environment variables set:
///
/// * `AWS_ACCESS_KEY_ID`
/// * `AWS_SECRET_ACCESS_KEY`
/// * `AWS_SESSION_TOKEN` (if any)
///
/// # Errors
///
/// Returns an error if:
///
/// * The default AWS SDK credential provider chain has no provider.
/// * Forwards [`ProvideCredentials::provide_credentials`]'s return values on failure.
fn s3_credential_env(
    runtime: &tokio::runtime::Handle,
    region: &str,
    auth: &AwsAuthentication,
) -> anyhow::Result<Vec<(&'static str, String)>> {
    /// The env var holding the AWS access key ID.
    const AWS_ACCESS_KEY_ID_ENV_VAR: &str = "AWS_ACCESS_KEY_ID";

    /// The env var holding the AWS secret access key.
    const AWS_SECRET_ACCESS_KEY_ENV_VAR: &str = "AWS_SECRET_ACCESS_KEY";

    /// The env var holding the AWS session token.
    const AWS_SESSION_TOKEN_ENV_VAR: &str = "AWS_SESSION_TOKEN";

    let (access_key_id, secret_access_key, session_token) = match auth {
        AwsAuthentication::Credentials { credentials } => (
            credentials.access_key_id.clone(),
            credentials.secret_access_key.clone(),
            credentials.session_token.clone(),
        ),
        AwsAuthentication::Default => {
            let sdk_config = runtime.block_on(
                aws_config::defaults(BehaviorVersion::latest())
                    .region(aws_sdk_s3::config::Region::new(region.to_string()))
                    .load(),
            );
            let provider = sdk_config
                .credentials_provider()
                .context("default AWS SDK credential provider is unavailable")?;
            let credentials = runtime
                .block_on(provider.provide_credentials())
                .context("failed to resolve credentials from the default AWS SDK provider chain")?;
            (
                credentials.access_key_id().to_string(),
                credentials.secret_access_key().to_string(),
                credentials
                    .session_token()
                    .map(std::string::ToString::to_string),
            )
        }
    };

    let mut env = vec![
        (AWS_ACCESS_KEY_ID_ENV_VAR, access_key_id),
        (AWS_SECRET_ACCESS_KEY_ENV_VAR, secret_access_key),
    ];
    if let Some(session_token) = session_token {
        env.push((AWS_SESSION_TOKEN_ENV_VAR, session_token));
    }
    Ok(env)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::num::NonZeroU16;
    use std::num::NonZeroU32;
    use std::path::Path;
    use std::path::PathBuf;

    use clp_rust_utils::clp_config::AwsAuthentication;
    use clp_rust_utils::clp_config::AwsCredentials;
    use clp_rust_utils::clp_config::S3Config;
    use clp_rust_utils::clp_config::package::config::ArchiveOutput;
    use clp_rust_utils::clp_config::package::config::ArchiveOutputStorage;
    use clp_rust_utils::clp_config::package::config::Package;
    use clp_rust_utils::clp_config::package::config::SpiderTaskExecutorConfig;
    use clp_rust_utils::clp_config::package::config::StorageEngine;
    use clp_rust_utils::task_io::query::ClpSQueryOption;
    use clp_rust_utils::types::ArchiveId;
    use clp_rust_utils::types::non_empty_string::ExpectedNonEmpty;
    use non_empty_string::NonEmptyString;

    use super::ArchiveSelector;
    use super::build_clp_s_search_args_for_network;
    use super::build_clp_s_search_args_for_result_cache;
    use super::resolve_archive_input;
    use super::run_clp_s_search;
    use super::s3_credential_env;

    const ARCHIVE_ID: &str = "018e90e5-8b2a-4a61-a2fc-cac799936caf";

    /// # Returns
    ///
    /// The id of the test archive.
    fn archive_id() -> ArchiveId {
        ARCHIVE_ID.parse().expect("valid archive UUID")
    }

    /// # Returns
    ///
    /// The query options of a query without time bounds, a result limit or case-insensitivity.
    fn unbounded_query_option() -> ClpSQueryOption {
        ClpSQueryOption {
            query_string: NonEmptyString::from_static_str("level: \"ERROR\""),
            max_num_results: None,
            begin_timestamp_millisecs: None,
            end_timestamp_millisecs: None,
            ignore_case: false,
        }
    }

    /// # Returns
    ///
    /// Static AWS credentials, with `session_token` if any.
    fn test_aws_authentication(session_token: Option<&str>) -> AwsAuthentication {
        AwsAuthentication::Credentials {
            credentials: AwsCredentials {
                access_key_id: "the-access-key".to_owned(),
                secret_access_key: "the-secret-key".to_owned(),
                session_token: session_token.map(str::to_owned),
            },
        }
    }

    /// # Returns
    ///
    /// A selector of the test archive in the file-system dataset directory `/archives/ds1`.
    fn directory_selector() -> ArchiveSelector {
        ArchiveSelector::Directory {
            path: PathBuf::from("/archives/ds1"),
            archive_id: archive_id(),
        }
    }

    /// # Returns
    ///
    /// An executor config for clp-s archives stored in S3, at `endpoint_url` if any.
    fn s3_backed_config(endpoint_url: Option<&'static str>) -> SpiderTaskExecutorConfig {
        SpiderTaskExecutorConfig {
            package: Package {
                storage_engine: StorageEngine::ClpS,
                ..Package::default()
            },
            archive_output: ArchiveOutput {
                storage: ArchiveOutputStorage::S3 {
                    staging_directory: "/wrong-staging-directory".to_owned(),
                    s3_config: S3Config {
                        bucket: NonEmptyString::from_static_str("bucket"),
                        region_code: None,
                        key_prefix: NonEmptyString::from_static_str("LIB1/"),
                        endpoint_url: endpoint_url.map(NonEmptyString::from_static_str),
                        aws_authentication: test_aws_authentication(None),
                    },
                },
                ..ArchiveOutput::default()
            },
            ..SpiderTaskExecutorConfig::default()
        }
    }

    /// # Returns
    ///
    /// A multi-thread runtime with one worker thread.
    ///
    /// # Panics
    ///
    /// Panics if the runtime can't be built.
    fn multi_thread_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a multi-thread runtime can be built")
    }

    #[test]
    fn args_carry_every_option_in_the_package_order() {
        let clp_s_query_option = ClpSQueryOption {
            max_num_results: Some(NonZeroU32::new(7).expect("7 is nonzero")),
            begin_timestamp_millisecs: Some(1_310_138_944_000),
            end_timestamp_millisecs: Some(1_311_208_074_120),
            ignore_case: true,
            ..unbounded_query_option()
        };
        assert_eq!(
            build_clp_s_search_args_for_result_cache(
                &directory_selector(),
                &clp_s_query_option,
                "mongodb://results-cache:27017/clp-query-results",
                42,
                "ds1",
            ),
            [
                "s",
                "/archives/ds1",
                "--archive-id",
                ARCHIVE_ID,
                "level: \"ERROR\"",
                "--tge",
                "1310138944000",
                "--tle",
                "1311208074120",
                "--ignore-case",
                "results-cache",
                "--uri",
                "mongodb://results-cache:27017/clp-query-results",
                "--collection",
                "42",
                "--max-num-results",
                "7",
                "--dataset",
                "ds1",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn args_omit_unset_options() {
        assert_eq!(
            build_clp_s_search_args_for_result_cache(
                &ArchiveSelector::ObjectUrl("https://bucket.s3.amazonaws.com/k".to_owned()),
                &unbounded_query_option(),
                "mongodb://results-cache:27017/clp-query-results",
                42,
                "default",
            ),
            [
                "s",
                "https://bucket.s3.amazonaws.com/k",
                "--auth",
                "s3",
                "level: \"ERROR\"",
                "results-cache",
                "--uri",
                "mongodb://results-cache:27017/clp-query-results",
                "--collection",
                "42",
                "--dataset",
                "default",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn network_args_carry_every_option_in_the_package_order() {
        const SESSION_TOKEN: &str = "6f1c1c64-1e0a-4be8-a7a6-6a0e4d3c2b1a";

        let session_token = SESSION_TOKEN.parse().expect("valid session token UUID");
        let port = NonZeroU16::new(52_000).expect("52000 is nonzero");
        let clp_s_query_option = ClpSQueryOption {
            max_num_results: Some(NonZeroU32::new(7).expect("7 is nonzero")),
            begin_timestamp_millisecs: Some(1_310_138_944_000),
            end_timestamp_millisecs: Some(1_311_208_074_120),
            ignore_case: true,
            ..unbounded_query_option()
        };
        assert_eq!(
            build_clp_s_search_args_for_network(
                &directory_selector(),
                &clp_s_query_option,
                "search-result-listener",
                port,
                session_token,
                12,
            ),
            [
                "s",
                "/archives/ds1",
                "--archive-id",
                ARCHIVE_ID,
                "level: \"ERROR\"",
                "--tge",
                "1310138944000",
                "--tle",
                "1311208074120",
                "--ignore-case",
                "network",
                "--host",
                "search-result-listener",
                "--port",
                "52000",
                "--session-token",
                SESSION_TOKEN,
                "--task-index",
                "12",
            ]
            .map(OsString::from)
        );
        assert_eq!(
            build_clp_s_search_args_for_network(
                &ArchiveSelector::ObjectUrl("https://bucket.s3.amazonaws.com/k".to_owned()),
                &unbounded_query_option(),
                "127.0.0.1",
                port,
                session_token,
                0,
            ),
            [
                "s",
                "https://bucket.s3.amazonaws.com/k",
                "--auth",
                "s3",
                "level: \"ERROR\"",
                "network",
                "--host",
                "127.0.0.1",
                "--port",
                "52000",
                "--session-token",
                SESSION_TOKEN,
                "--task-index",
                "0",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn fs_inputs_join_the_dataset_and_need_no_credentials() -> anyhow::Result<()> {
        let runtime = multi_thread_runtime();
        let config = SpiderTaskExecutorConfig {
            package: Package {
                storage_engine: StorageEngine::ClpS,
                ..Package::default()
            },
            archive_output: ArchiveOutput {
                storage: ArchiveOutputStorage::Fs {
                    directory: "var/data/archives".to_owned(),
                },
                ..ArchiveOutput::default()
            },
            ..SpiderTaskExecutorConfig::default()
        };
        let (selector, credential_env) = resolve_archive_input(
            runtime.handle(),
            Path::new("/clp"),
            &config,
            "ds1",
            archive_id(),
        )?;
        assert_eq!(
            selector,
            ArchiveSelector::Directory {
                path: PathBuf::from("/clp/var/data/archives/ds1"),
                archive_id: archive_id(),
            }
        );
        assert_eq!(credential_env, &[]);
        Ok(())
    }

    #[test]
    fn s3_inputs_build_object_urls_and_credentials() -> anyhow::Result<()> {
        let runtime = multi_thread_runtime();
        let (selector, credential_env) = resolve_archive_input(
            runtime.handle(),
            Path::new("/clp"),
            &s3_backed_config(None),
            "ds1",
            archive_id(),
        )?;
        assert_eq!(
            selector,
            ArchiveSelector::ObjectUrl(format!(
                "https://bucket.s3.amazonaws.com/LIB1/ds1/{ARCHIVE_ID}"
            ))
        );
        assert_eq!(
            credential_env,
            [
                ("AWS_ACCESS_KEY_ID", "the-access-key".to_owned()),
                ("AWS_SECRET_ACCESS_KEY", "the-secret-key".to_owned()),
            ]
        );

        let (selector, _) = resolve_archive_input(
            runtime.handle(),
            Path::new("/clp"),
            &s3_backed_config(Some("http://minio:9000")),
            "ds1",
            archive_id(),
        )?;
        assert_eq!(
            selector,
            ArchiveSelector::ObjectUrl(format!("http://minio:9000/bucket/LIB1/ds1/{ARCHIVE_ID}"))
        );
        Ok(())
    }

    #[test]
    fn session_tokens_are_forwarded() -> anyhow::Result<()> {
        let runtime = multi_thread_runtime();
        assert_eq!(
            s3_credential_env(
                runtime.handle(),
                "us-east-1",
                &test_aws_authentication(Some("the-session-token")),
            )?,
            [
                ("AWS_ACCESS_KEY_ID", "the-access-key".to_owned()),
                ("AWS_SECRET_ACCESS_KEY", "the-secret-key".to_owned()),
                ("AWS_SESSION_TOKEN", "the-session-token".to_owned()),
            ]
        );
        Ok(())
    }

    #[test]
    fn failing_or_missing_binaries_are_errors() {
        let error = run_clp_s_search(Path::new("/bin/false"), Vec::new(), &[])
            .expect_err("a non-zero exit is an error");
        assert!(error.to_string().contains("exited on error"), "{error}");
        let error = run_clp_s_search(Path::new("/nonexistent/clp-s"), Vec::new(), &[])
            .expect_err("a missing binary is an error");
        assert!(error.to_string().contains("failed to spawn"), "{error}");
        run_clp_s_search(Path::new("/bin/true"), Vec::new(), &[])
            .expect("a zero exit is a success");
    }
}
