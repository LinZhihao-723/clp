//! The `compress` task: runs clp-s on an S3 partition and uploads the produced archives to S3.

use std::{
    ffi::OsString,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::Context;
use clp_rust_utils::{
    aws::AWS_DEFAULT_REGION,
    clp_config::{
        AwsAuthentication,
        S3Config,
        package::config::{ArchiveOutput, ArchiveOutputStorage, Database, SpiderTaskExecutorConfig},
    },
    dataset::resolve_dataset_name,
    s3::{create_new_client, generate_s3_url},
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
/// Returns an error if any compression step fails: running clp-s, uploading an archive to S3,
/// running the indexer, or deleting a local archive.
pub fn compress(
    ctx: &spider_tdl::TaskContext,
    config: &SpiderTaskExecutorConfig,
    clp_s_option: &ClpSCompressionOption,
    dataset: Option<String>,
    input_source: S3InputSource,
) -> anyhow::Result<CompressionTaskOutput> {
    let clp_home = clp_home()?;
    let list_path = config.abs_tmp_directory(&clp_home).join(format!(
        "compression-{}-{}-{}-log-paths.txt",
        ctx.job_id, ctx.task_id, ctx.task_instance_id,
    ));
    std::fs::write(&list_path, build_s3_logs_list(&input_source))
        .with_context(|| format!("failed to write S3 logs list to {}", list_path.display()))?;

    let S3InputSource {
        aws_authentication, ..
    } = input_source;
    let credential_env = s3_credential_env(&aws_authentication);

    let dataset = dataset.unwrap_or_else(|| resolve_dataset_name(None).to_owned());
    let s3_config = s3_archive_output(config)?;
    let archive_dir = config.abs_archive_output_staging(&clp_home).join(&dataset);

    let clp_s_bin = clp_binary_path(&clp_home, "clp-s");
    let indexer_bin = clp_binary_path(&clp_home, "indexer");

    let region = s3_config
        .region_code
        .as_ref()
        .map_or(AWS_DEFAULT_REGION, NonEmptyString::as_str);
    let client = super::runtime().block_on(create_new_client(
        region,
        s3_config.endpoint_url.as_ref(),
        &s3_config.aws_authentication,
    ));
    let bucket = s3_config.bucket.to_string();

    let mut archives = Vec::new();
    let mut finishers = tokio::task::JoinSet::new();

    let run_result = run_clp_s(
        &clp_s_bin,
        &archive_dir,
        clp_s_option,
        &list_path,
        &credential_env,
        |archive| {
            let local_path = archive_dir.join(&archive.id);
            let key = create_archive_s3_key(&config.archive_output, &dataset, &archive.id);
            let client = client.clone();
            let bucket = bucket.clone();
            let db_config = config.database.clone();
            let indexer_bin = indexer_bin.clone();
            let dataset = dataset.clone();
            finishers.spawn_on(
                async move {
                    let index_path = local_path.clone();
                    let index = tokio::task::spawn_blocking(move || {
                        run_indexer(&indexer_bin, &db_config, &dataset, &index_path)
                    });
                    let (upload_result, index_result) =
                        tokio::join!(super::put_file(&client, &bucket, &key, &local_path), index,);
                    upload_result.with_context(|| {
                        format!("failed to upload archive to s3://{bucket}/{key}")
                    })?;
                    index_result.map_err(|e| anyhow::anyhow!("indexer task panicked: {e}"))??;
                    tokio::fs::remove_file(&local_path).await.with_context(|| {
                        format!("failed to delete local archive {}", local_path.display())
                    })?;
                    anyhow::Ok(())
                },
                super::runtime().handle(),
            );
            archives.push(archive);
            Ok(())
        },
    );

    let finish_error = super::runtime().block_on(async {
        let mut first_error: Option<anyhow::Error> = None;
        while let Some(joined) = finishers.join_next().await {
            let result = joined
                .map_err(|e| anyhow::anyhow!("finishing task panicked: {e}"))
                .and_then(|r| r);
            if let Err(e) = result
                && first_error.is_none()
            {
                first_error = Some(e);
            }
        }
        first_error
    });

    run_result?;
    if let Some(e) = finish_error {
        return Err(e);
    }

    Ok(CompressionTaskOutput {
        dataset: Some(dataset),
        archives,
    })
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

/// Resolves the AWS credential env vars clp-s needs to access the S3 objects.
///
/// # Returns
///
/// The env-var name/value pairs for [`AwsAuthentication::Credentials`], or an empty vector for
/// [`AwsAuthentication::Default`] (which assumes credentials are already in the ambient env).
fn s3_credential_env(auth: &AwsAuthentication) -> Vec<(&'static str, String)> {
    /// The env var holding the AWS access key ID.
    const AWS_ACCESS_KEY_ID_ENV_VAR: &str = "AWS_ACCESS_KEY_ID";

    /// The env var holding the AWS secret access key.
    const AWS_SECRET_ACCESS_KEY_ENV_VAR: &str = "AWS_SECRET_ACCESS_KEY";

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

/// Reads the CLP installation's home directory from [`CLP_HOME_ENV_VAR`].
///
/// # Returns
///
/// The CLP home directory.
///
/// # Errors
///
/// Returns an error if [`CLP_HOME_ENV_VAR`] is unset or not valid Unicode.
fn clp_home() -> anyhow::Result<PathBuf> {
    let clp_home = std::env::var(CLP_HOME_ENV_VAR)
        .context("failed to read the CLP_HOME environment variable")?;
    Ok(PathBuf::from(clp_home))
}

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

/// Resolves the path of a CLP binary under `clp_home`, joining `bin/{binary}`.
///
/// # Returns
///
/// The path to the named binary under the CLP installation.
fn clp_binary_path(clp_home: &Path, binary: &str) -> PathBuf {
    clp_home.join("bin").join(binary)
}

/// Resolves the S3 config the archives are uploaded to from `config`.
///
/// # Returns
///
/// The S3 config the archives are uploaded to.
///
/// # Errors
///
/// Returns an error if `config`'s archive output is not S3-backed, which this flow requires.
fn s3_archive_output(config: &SpiderTaskExecutorConfig) -> anyhow::Result<&S3Config> {
    match &config.archive_output.storage {
        ArchiveOutputStorage::S3 { s3_config, .. } => Ok(s3_config),
        ArchiveOutputStorage::Fs { .. } => {
            anyhow::bail!("S3 archive output is required for the S3 compression flow")
        }
    }
}

/// Builds the S3 object key for an archive by appending `archive_id` to
/// [`ArchiveOutput::dataset_archive_storage_directory`].
///
/// # Returns
///
/// The archive's S3 object key.
fn create_archive_s3_key(archive_output: &ArchiveOutput, dataset: &str, archive_id: &str) -> String {
    format!(
        "{}/{archive_id}",
        archive_output.dataset_archive_storage_directory(Some(dataset))
    )
}

/// Builds the `indexer` command-line arguments for indexing one archive.
///
/// # Returns
///
/// The ordered `indexer` arguments.
fn build_indexer_args(database: &Database, dataset: &str, archive_path: &Path) -> Vec<OsString> {
    vec![
        OsString::from("--db-type"),
        // clp-core's indexer only supports MySQL, regardless of the configured DB engine.
        OsString::from("mysql"),
        OsString::from("--db-host"),
        OsString::from(&database.host),
        OsString::from("--db-port"),
        OsString::from(database.port.to_string()),
        OsString::from("--db-name"),
        OsString::from(&database.names.clp),
        OsString::from("--db-table-prefix"),
        OsString::from(&database.table_prefix),
        OsString::from(dataset),
        archive_path.as_os_str().to_os_string(),
    ]
}

/// Runs the `indexer` on one archive, blocking until it exits.
///
/// # Returns
///
/// `()` once the indexer exits successfully.
///
/// # Errors
///
/// Returns an error if the indexer fails to spawn or exits with a non-zero status.
fn run_indexer(
    indexer_bin: &Path,
    database: &Database,
    dataset: &str,
    archive_path: &Path,
) -> anyhow::Result<()> {
    let status = Command::new(indexer_bin)
        .args(build_indexer_args(database, dataset, archive_path))
        .status()
        .with_context(|| format!("failed to spawn indexer at {}", indexer_bin.display()))?;
    if !status.success() {
        anyhow::bail!(
            "indexer exited with {status} for archive {}",
            archive_path.display()
        );
    }
    Ok(())
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

    use clp_rust_utils::clp_config::{
        AwsAuthentication,
        AwsCredentials,
        S3Config,
        package::config::{
            ArchiveOutput,
            ArchiveOutputStorage,
            ClpDbNames,
            Database,
            DatabaseEngine,
        },
    };
    use non_empty_string::NonEmptyString;

    use super::{
        build_clp_s_args,
        build_indexer_args,
        build_s3_logs_list,
        create_archive_s3_key,
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
                ("AWS_ACCESS_KEY_ID", "the-access-key".to_string()),
                ("AWS_SECRET_ACCESS_KEY", "the-secret-key".to_string()),
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

    #[test]
    fn archive_s3_key_joins_prefix_dataset_and_id() {
        let archive_output = ArchiveOutput {
            storage: ArchiveOutputStorage::S3 {
                staging_directory: "var/data/staged-archives".to_owned(),
                s3_config: S3Config {
                    bucket: NonEmptyString::try_from("bucket".to_string())
                        .expect("bucket is non-empty"),
                    region_code: None,
                    key_prefix: NonEmptyString::try_from("LIB1/".to_string())
                        .expect("key prefix is non-empty"),
                    endpoint_url: None,
                    aws_authentication: AwsAuthentication::Default,
                },
            },
            ..ArchiveOutput::default()
        };

        assert_eq!(
            create_archive_s3_key(&archive_output, "default", "abc"),
            "LIB1/default/abc"
        );
    }

    #[test]
    fn build_indexer_args_uses_mysql_and_expected_order() {
        let database = Database {
            engine: DatabaseEngine::MariaDb,
            host: "db-host".to_string(),
            port: 3306,
            names: ClpDbNames {
                clp: "clp-db".to_string(),
                spider: "spider-db".to_string(),
            },
            table_prefix: "clp_".to_string(),
        };

        assert_eq!(
            build_indexer_args(&database, "default", Path::new("/archives/abc")),
            vec![
                OsString::from("--db-type"),
                OsString::from("mysql"),
                OsString::from("--db-host"),
                OsString::from("db-host"),
                OsString::from("--db-port"),
                OsString::from("3306"),
                OsString::from("--db-name"),
                OsString::from("clp-db"),
                OsString::from("--db-table-prefix"),
                OsString::from("clp_"),
                OsString::from("default"),
                OsString::from("/archives/abc"),
            ]
        );
    }
}
