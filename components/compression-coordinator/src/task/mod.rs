//! Compression task implementations and the Spider task executor config they read at runtime.

use std::sync::OnceLock;

use anyhow::Context;
use clp_rust_utils::clp_config::package::config::SpiderTaskExecutorConfig;

pub mod commit;
pub mod s3_compression;

/// Initializes the process-wide Tokio runtime the compression tasks use to drive async S3 I/O.
///
/// Idempotent: a no-op once the runtime is initialized.
///
/// # Errors
///
/// Returns an error if the runtime fails to build.
pub fn init_runtime() -> anyhow::Result<()> {
    if TOKIO_RUNTIME.get().is_some() {
        return Ok(());
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build the compression-coordinator Tokio runtime")?;
    let _ = TOKIO_RUNTIME.set(rt);
    Ok(())
}

/// Initializes the process-wide Spider task executor config from `CLP_CONFIG_PATH`.
///
/// Idempotent: a no-op once the config is initialized.
///
/// # Errors
///
/// Returns an error if `CLP_CONFIG_PATH` is unset or not valid Unicode, or if the YAML file at that
/// path cannot be read or parsed.
pub fn init_config() -> anyhow::Result<()> {
    if SPIDER_TASK_EXECUTOR_CONFIG.get().is_some() {
        return Ok(());
    }
    let cfg = load_spider_task_executor_config_from_env()?;
    let _ = SPIDER_TASK_EXECUTOR_CONFIG.set(cfg);
    Ok(())
}

/// Returns the process-wide Spider task executor config.
///
/// # Returns
///
/// A reference to the cached [`SpiderTaskExecutorConfig`].
///
/// # Panics
///
/// Panics if the config has not been initialized by the TDL package init hook.
#[must_use]
pub fn spider_task_executor_config() -> &'static SpiderTaskExecutorConfig {
    SPIDER_TASK_EXECUTOR_CONFIG.get().expect(
        "Spider task executor config not initialized; the TDL package init hook must run before \
         any task",
    )
}

/// Returns the process-wide Tokio runtime the compression tasks use to drive async S3 I/O.
///
/// # Returns
///
/// A reference to the cached multi-threaded [`tokio::runtime::Runtime`].
///
/// # Panics
///
/// Panics if the runtime has not been initialized by the TDL package init hook.
#[must_use]
pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    TOKIO_RUNTIME
        .get()
        .expect("Tokio runtime not initialized; the TDL package init hook must run before any task")
}

/// The env var holding the path to the Spider task executor config YAML.
const CLP_CONFIG_PATH_ENV_VAR: &str = "CLP_CONFIG_PATH";

/// Process-wide multi-threaded Tokio runtime, initialized by [`init_runtime`].
static TOKIO_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// Process-wide cache of the Spider task executor config, initialized by [`init_config`].
static SPIDER_TASK_EXECUTOR_CONFIG: OnceLock<SpiderTaskExecutorConfig> = OnceLock::new();

/// Loads the [`SpiderTaskExecutorConfig`] from the YAML file at the path named by
/// [`CLP_CONFIG_PATH_ENV_VAR`].
///
/// # Returns
///
/// The deserialized [`SpiderTaskExecutorConfig`].
///
/// # Errors
///
/// Returns an error if [`CLP_CONFIG_PATH_ENV_VAR`] is unset or not valid Unicode, or if the YAML
/// file at that path cannot be read or parsed.
fn load_spider_task_executor_config_from_env() -> anyhow::Result<SpiderTaskExecutorConfig> {
    let path = std::env::var(CLP_CONFIG_PATH_ENV_VAR).with_context(|| {
        format!("failed to read the `{CLP_CONFIG_PATH_ENV_VAR}` environment variable")
    })?;
    clp_rust_utils::serde::yaml::from_path(&path)
        .with_context(|| format!("failed to load the Spider task executor config from `{path}`"))
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
