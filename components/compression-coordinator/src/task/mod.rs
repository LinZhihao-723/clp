//! Compression task implementations and the Spider task executor config they read at runtime.

use std::sync::LazyLock;

use clp_rust_utils::clp_config::package::config::SpiderTaskExecutorConfig;

pub mod commit;
pub mod s3_compression;

/// Returns the process-wide Spider task executor config, loading it from `CLP_CONFIG_PATH` on first
/// access.
///
/// The config is loaded once, on first access; subsequent calls return the cached value.
///
/// # Returns
///
/// A reference to the cached [`SpiderTaskExecutorConfig`].
///
/// # Panics
///
/// Panics if `CLP_CONFIG_PATH` is unset or not valid Unicode, or if the YAML file at that path
/// cannot be read or parsed.
#[must_use]
pub fn spider_task_executor_config() -> &'static SpiderTaskExecutorConfig {
    &SPIDER_TASK_EXECUTOR_CONFIG
}

/// Returns the process-wide Tokio runtime the compression tasks use to drive async S3 I/O.
///
/// # Returns
///
/// A reference to the cached multi-threaded [`tokio::runtime::Runtime`].
///
/// # Panics
///
/// Panics if the runtime failed to build on first access.
#[must_use]
pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    &TOKIO_RUNTIME
}

/// The env var holding the path to the Spider task executor config YAML.
const CLP_CONFIG_PATH_ENV_VAR: &str = "CLP_CONFIG_PATH";

/// Process-wide multi-threaded Tokio runtime, built on first access.
static TOKIO_RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build the compression-coordinator Tokio runtime")
});

/// Process-wide cache of the Spider task executor config, populated on first access from
/// [`load_spider_task_executor_config_from_env`].
static SPIDER_TASK_EXECUTOR_CONFIG: LazyLock<SpiderTaskExecutorConfig> =
    LazyLock::new(load_spider_task_executor_config_from_env);

/// Loads the [`SpiderTaskExecutorConfig`] from the YAML file at the path named by
/// [`CLP_CONFIG_PATH_ENV_VAR`].
///
/// # Returns
///
/// The deserialized [`SpiderTaskExecutorConfig`].
///
/// # Panics
///
/// Panics if [`CLP_CONFIG_PATH_ENV_VAR`] is unset or not valid Unicode, or if the YAML file at that
/// path cannot be read or parsed.
fn load_spider_task_executor_config_from_env() -> SpiderTaskExecutorConfig {
    let path = std::env::var(CLP_CONFIG_PATH_ENV_VAR).unwrap_or_else(|e| {
        panic!("failed to read the `{CLP_CONFIG_PATH_ENV_VAR}` environment variable: {e}")
    });
    clp_rust_utils::serde::yaml::from_path(&path).unwrap_or_else(|e| {
        panic!("failed to load the Spider task executor config from `{path}`: {e}")
    })
}

/// Uploads a local file to S3 with a single `PutObject` (mirror of Python's `s3_put`).
///
/// # Returns
///
/// `()` once the object has been uploaded.
///
/// # Errors
///
/// Returns an error if `src` cannot be read into a body stream or the `PutObject` request fails.
async fn put_file(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
    src: &std::path::Path,
) -> anyhow::Result<()> {
    use anyhow::Context;

    let body = aws_sdk_s3::primitives::ByteStream::from_path(src)
        .await
        .with_context(|| format!("failed to read {} for upload", src.display()))?;
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(body)
        .send()
        .await
        .with_context(|| format!("failed to upload to s3://{bucket}/{key}"))?;
    Ok(())
}
