//! The `compress` task: runs clp-s on an S3 partition and uploads the produced archives to S3.

use std::{
    ffi::OsString,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::Context;
use clp_rust_utils::{
    clp_config::{
        AwsAuthentication,
        package::config::{ArchiveOutputStorage, SpiderTaskExecutorConfig},
    },
    s3::generate_s3_url,
};
use non_empty_string::NonEmptyString;

use crate::task_io::{
    ArchiveMetadata,
    ClpSCompressionOption,
    CompressionTaskOutput,
    S3InputSource,
};

/// Compresses one partition of S3 objects into archives, uploads them to S3, and returns their
/// metadata for the commit task.
///
/// A pure worker function called by a spider-tdl task wrapper, which formats any returned
/// `anyhow::Error` into a user-space TDL error.
///
/// # Returns
///
/// The metadata of every archive this task produced.
///
/// # Errors
///
/// Returns an error if any compression step fails (e.g., running clp-s or uploading to S3).
pub fn compress(
    ctx: &spider_tdl::TaskContext,
    config: &SpiderTaskExecutorConfig,
    clp_s_option: &ClpSCompressionOption,
    dataset: Option<String>,
    input_source: S3InputSource,
) -> anyhow::Result<CompressionTaskOutput> {
    let list_path = std::path::Path::new(&config.tmp_directory).join(format!(
        "compression-{}-{}-{}-log-paths.txt",
        ctx.job_id, ctx.task_id, ctx.task_instance_id,
    ));
    std::fs::write(&list_path, build_s3_logs_list(&input_source))
        .with_context(|| format!("failed to write S3 logs list to {}", list_path.display()))?;

    let credential_env = s3_credential_env(&input_source.aws_authentication);

    let clp_s_bin = clp_s_binary_path()?;
    let archive_dir = archive_staging_dir(config, dataset.as_deref())?;

    let mut archives = Vec::new();
    run_clp_s(
        &clp_s_bin,
        &archive_dir,
        clp_s_option,
        &list_path,
        &credential_env,
        |archive| {
            archives.push(archive);
            Ok(())
        },
    )?;

    let _ = (dataset, input_source, &archives);
    todo!(
        "steps 3-4: upload archives to S3, index, delete local; then return CompressionTaskOutput \
         {{ dataset, archives }}"
    )
}

/// Builds the `--files-from` list of S3 object URLs for clp-s.
///
/// # Returns
///
/// The newline-terminated list of object URLs, one per object key in `input_source`.
fn build_s3_logs_list(input_source: &S3InputSource) -> String {
    let endpoint = input_source
        .endpoint_url
        .as_ref()
        .map(NonEmptyString::as_str);
    let region = input_source
        .region_code
        .as_ref()
        .map(NonEmptyString::as_str);
    let bucket = input_source.bucket.as_str();

    let mut list = String::new();
    for object_key in &input_source.object_keys {
        list.push_str(&generate_s3_url(endpoint, region, bucket, object_key));
        list.push('\n');
    }
    list
}

/// The env var holding the AWS access key ID.
const AWS_ACCESS_KEY_ID_ENV_VAR: &str = "AWS_ACCESS_KEY_ID";

/// The env var holding the AWS secret access key.
const AWS_SECRET_ACCESS_KEY_ENV_VAR: &str = "AWS_SECRET_ACCESS_KEY";

/// Resolves the AWS credential env vars clp-s needs to access the S3 objects.
///
/// # Returns
///
/// The env-var name/value pairs for [`AwsAuthentication::Credentials`], or an empty vector for
/// [`AwsAuthentication::Default`] (which assumes credentials are already in the ambient env).
fn s3_credential_env(auth: &AwsAuthentication) -> Vec<(&'static str, String)> {
    match auth {
        AwsAuthentication::Credentials { credentials } => vec![
            (AWS_ACCESS_KEY_ID_ENV_VAR, credentials.access_key_id.clone()),
            (
                AWS_SECRET_ACCESS_KEY_ENV_VAR,
                credentials.secret_access_key.clone(),
            ),
        ],
        AwsAuthentication::Default => Vec::new(),
    }
}

/// The env var holding the CLP installation's home directory.
const CLP_HOME_ENV_VAR: &str = "CLP_HOME";

/// The path of the clp-s binary relative to [`CLP_HOME_ENV_VAR`].
const CLP_S_BINARY_SUBPATH: &str = "bin/clp-s";

/// Parses a single clp-s `--print-archive-stats` stdout line into an [`ArchiveMetadata`].
///
/// clp-s emits a superset of [`ArchiveMetadata`]'s fields per line; unknown keys are ignored.
///
/// # Returns
///
/// The parsed [`ArchiveMetadata`].
///
/// # Errors
///
/// Returns an error if `line` is not valid JSON matching [`ArchiveMetadata`].
fn parse_archive_stats(line: &str) -> anyhow::Result<ArchiveMetadata> {
    serde_json::from_str(line)
        .with_context(|| format!("failed to parse clp-s archive stats: {line}"))
}

/// Builds the clp-s command-line arguments for a structured, single-file-archive S3 compression
/// run.
///
/// # Returns
///
/// The ordered clp-s arguments.
fn build_clp_s_args(
    archive_dir: &Path,
    files_from_path: &Path,
    clp_s_option: &ClpSCompressionOption,
) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("c"),
        archive_dir.as_os_str().to_os_string(),
        OsString::from("--print-archive-stats"),
        OsString::from("--target-encoded-size"),
        OsString::from(clp_s_option.target_encoded_size.to_string()),
        OsString::from("--compression-level"),
        OsString::from(clp_s_option.compression_level.to_string()),
        OsString::from("--auth"),
        OsString::from("s3"),
        OsString::from("--single-file-archive"),
    ];
    if let Some(timestamp_key) = &clp_s_option.timestamp_key {
        args.push(OsString::from("--timestamp-key"));
        args.push(OsString::from(timestamp_key));
    }
    args.push(OsString::from("--files-from"));
    args.push(files_from_path.as_os_str().to_os_string());
    args
}

/// Resolves the clp-s binary path from [`CLP_HOME_ENV_VAR`].
///
/// # Returns
///
/// The path to the clp-s binary.
///
/// # Errors
///
/// Returns an error if [`CLP_HOME_ENV_VAR`] is unset or not valid Unicode.
fn clp_s_binary_path() -> anyhow::Result<PathBuf> {
    let clp_home = std::env::var(CLP_HOME_ENV_VAR)
        .context("failed to read the CLP_HOME environment variable")?;
    Ok(Path::new(&clp_home).join(CLP_S_BINARY_SUBPATH))
}

/// Resolves the local staging directory the archives are written to before upload.
///
/// The dataset, when set, is appended to the S3 staging directory (matching Python's
/// `archive_output_dir / dataset`).
///
/// # Returns
///
/// The archive staging directory.
///
/// # Errors
///
/// Returns an error if `config`'s archive output is not S3-backed, which this flow requires.
fn archive_staging_dir(
    config: &SpiderTaskExecutorConfig,
    dataset: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let base = match &config.archive_output.storage {
        ArchiveOutputStorage::S3 {
            staging_directory, ..
        } => Path::new(staging_directory),
        ArchiveOutputStorage::Fs { .. } => {
            anyhow::bail!("S3 archive output is required for the S3 compression flow")
        }
    };
    Ok(dataset.map_or_else(|| base.to_path_buf(), |dataset| base.join(dataset)))
}

/// Runs clp-s, invoking `on_archive` for each archive it reports on stdout.
///
/// Spawns clp-s with the resolved arguments and credential env vars, draining stderr on a dedicated
/// thread to avoid a pipe deadlock while stdout is streamed line by line. Each stdout line is
/// parsed into an [`ArchiveMetadata`] and forwarded to `on_archive`. If `on_archive` fails, clp-s
/// is killed and reaped before the error is returned.
///
/// # Returns
///
/// `()` once clp-s exits successfully and every reported archive has been forwarded.
///
/// # Errors
///
/// Returns an error if clp-s fails to spawn, if `on_archive` fails, if clp-s exits with a non-zero
/// status (the error includes the captured stderr), or if reaping clp-s or reading stderr fails.
/// Forwards [`parse_archive_stats`]'s errors.
///
/// # Panics
///
/// Panics if the stderr-reader thread panicked.
fn run_clp_s(
    clp_s_bin: &Path,
    archive_dir: &Path,
    clp_s_option: &ClpSCompressionOption,
    files_from_path: &Path,
    credential_env: &[(&'static str, String)],
    mut on_archive: impl FnMut(ArchiveMetadata) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut child = Command::new(clp_s_bin)
        .args(build_clp_s_args(archive_dir, files_from_path, clp_s_option))
        .envs(credential_env.iter().cloned())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn clp-s at {}", clp_s_bin.display()))?;

    let mut stderr = child
        .stderr
        .take()
        .context("clp-s stderr was not captured")?;
    let stderr_reader = std::thread::spawn(move || {
        let mut buffer = String::new();
        stderr.read_to_string(&mut buffer).map(|_| buffer)
    });

    let stdout = child
        .stdout
        .take()
        .context("clp-s stdout was not captured")?;
    for line in BufReader::new(stdout).lines() {
        let line = line.context("failed to read a line from clp-s stdout")?;
        let forward_result = parse_archive_stats(&line).and_then(&mut on_archive);
        if let Err(e) = forward_result {
            let _ = child.kill();
            let _ = stderr_reader.join();
            let _ = child.wait();
            return Err(e.context("clp-s was terminated while processing its output"));
        }
    }

    let status = child.wait().context("failed to wait for clp-s to exit")?;
    let stderr = stderr_reader
        .join()
        .expect("stderr reader thread panicked")
        .context("failed to read clp-s stderr")?;
    if !status.success() {
        anyhow::bail!("clp-s exited with {status}:\n{stderr}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::Path};

    use clp_rust_utils::clp_config::{AwsAuthentication, AwsCredentials};
    use non_empty_string::NonEmptyString;

    use super::{
        AWS_ACCESS_KEY_ID_ENV_VAR,
        AWS_SECRET_ACCESS_KEY_ENV_VAR,
        build_clp_s_args,
        build_s3_logs_list,
        parse_archive_stats,
        s3_credential_env,
    };
    use crate::task_io::{ArchiveMetadata, ClpSCompressionOption, S3InputSource};

    #[test]
    fn build_s3_logs_list_default_endpoint() {
        let input_source = S3InputSource {
            endpoint_url: None,
            region_code: Some(
                NonEmptyString::try_from("us-east-1".to_string())
                    .expect("region code is non-empty"),
            ),
            bucket: NonEmptyString::try_from("logs".to_string()).expect("bucket is non-empty"),
            aws_authentication: AwsAuthentication::Default,
            object_keys: vec!["a/b.json".to_string(), "c/d.json".to_string()],
        };

        assert_eq!(
            build_s3_logs_list(&input_source),
            "https://logs.s3.us-east-1.amazonaws.com/a/b.json\n\
             https://logs.s3.us-east-1.amazonaws.com/c/d.json\n"
        );
    }

    #[test]
    fn s3_credential_env_default() {
        assert_eq!(s3_credential_env(&AwsAuthentication::Default), Vec::new());
    }

    #[test]
    fn s3_credential_env_credentials() {
        let auth = AwsAuthentication::Credentials {
            credentials: AwsCredentials {
                access_key_id: "the-access-key".to_string(),
                secret_access_key: "the-secret-key".to_string(),
            },
        };

        assert_eq!(
            s3_credential_env(&auth),
            vec![
                (AWS_ACCESS_KEY_ID_ENV_VAR, "the-access-key".to_string()),
                (AWS_SECRET_ACCESS_KEY_ENV_VAR, "the-secret-key".to_string()),
            ]
        );
    }

    #[test]
    fn parse_archive_stats_ignores_extra_keys() {
        let line = concat!(
            r#"{"id":"abc","begin_timestamp":10,"end_timestamp":20,"#,
            r#""uncompressed_size":100,"size":40,"is_split":false,"range_index":{}}"#,
        );

        assert_eq!(
            parse_archive_stats(line).expect("valid archive stats line"),
            ArchiveMetadata {
                id: "abc".to_string(),
                begin_timestamp: 10,
                end_timestamp: 20,
                size: 40,
                uncompressed_size: 100,
            }
        );
    }

    #[test]
    fn parse_archive_stats_rejects_malformed_line() {
        assert!(parse_archive_stats("not json").is_err());
    }

    #[test]
    fn build_clp_s_args_with_timestamp_key() {
        let clp_s_option = ClpSCompressionOption {
            target_encoded_size: 268_435_456,
            compression_level: 3,
            timestamp_key: Some("ts".to_string()),
        };

        assert_eq!(
            build_clp_s_args(
                Path::new("/archives"),
                Path::new("/tmp/log-paths.txt"),
                &clp_s_option
            ),
            vec![
                OsString::from("c"),
                OsString::from("/archives"),
                OsString::from("--print-archive-stats"),
                OsString::from("--target-encoded-size"),
                OsString::from("268435456"),
                OsString::from("--compression-level"),
                OsString::from("3"),
                OsString::from("--auth"),
                OsString::from("s3"),
                OsString::from("--single-file-archive"),
                OsString::from("--timestamp-key"),
                OsString::from("ts"),
                OsString::from("--files-from"),
                OsString::from("/tmp/log-paths.txt"),
            ]
        );
    }

    #[test]
    fn build_clp_s_args_without_timestamp_key() {
        let clp_s_option = ClpSCompressionOption {
            target_encoded_size: 268_435_456,
            compression_level: 3,
            timestamp_key: None,
        };

        assert_eq!(
            build_clp_s_args(
                Path::new("/archives"),
                Path::new("/tmp/log-paths.txt"),
                &clp_s_option
            ),
            vec![
                OsString::from("c"),
                OsString::from("/archives"),
                OsString::from("--print-archive-stats"),
                OsString::from("--target-encoded-size"),
                OsString::from("268435456"),
                OsString::from("--compression-level"),
                OsString::from("3"),
                OsString::from("--auth"),
                OsString::from("s3"),
                OsString::from("--single-file-archive"),
                OsString::from("--files-from"),
                OsString::from("/tmp/log-paths.txt"),
            ]
        );
    }
}
