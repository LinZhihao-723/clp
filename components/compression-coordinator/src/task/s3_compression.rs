//! The `compress` task: runs clp-s on an S3 partition and uploads the produced archives to S3.

use anyhow::Context;
use clp_rust_utils::{clp_config::AwsAuthentication, s3::generate_s3_url};
use non_empty_string::NonEmptyString;

use crate::task_io::{ClpSCompressionOption, CompressionTaskOutput, DbConfig, S3InputSource};

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
    clp_s_option: &ClpSCompressionOption,
    dataset: Option<String>,
    db_config: &DbConfig,
    input_source: S3InputSource,
) -> anyhow::Result<CompressionTaskOutput> {
    let worker_config =
        super::worker_config().map_err(|e| anyhow::anyhow!("failed to load worker config: {e}"))?;

    let list_path = std::path::Path::new(&worker_config.tmp_directory).join(format!(
        "compression-{}-{}-{}-log-paths.txt",
        ctx.job_id, ctx.task_id, ctx.task_instance_id,
    ));
    std::fs::write(&list_path, build_s3_logs_list(&input_source))
        .with_context(|| format!("failed to write S3 logs list to {}", list_path.display()))?;

    let credential_env = s3_credential_env(&input_source.aws_authentication);

    let _ = (
        clp_s_option,
        dataset,
        db_config,
        input_source,
        &list_path,
        &credential_env,
    );
    todo!("steps 2-4: run clp-s, upload archives to S3, index, and delete the local archives")
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

#[cfg(test)]
mod tests {
    use clp_rust_utils::clp_config::{AwsAuthentication, AwsCredentials};
    use non_empty_string::NonEmptyString;

    use super::{
        AWS_ACCESS_KEY_ID_ENV_VAR,
        AWS_SECRET_ACCESS_KEY_ENV_VAR,
        build_s3_logs_list,
        s3_credential_env,
    };
    use crate::task_io::S3InputSource;

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
}
