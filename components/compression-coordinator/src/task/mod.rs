//! Compression task implementations and the Spider task executor config they read at runtime.

use std::sync::LazyLock;

use clp_rust_utils::clp_config::package::config::SpiderTaskExecutorConfig;

pub mod s3_compression;

/// Returns the process-wide Spider task executor config, loading it from `CLP_CONFIG_PATH` on first
/// access.
///
/// The config is loaded once, on first access; subsequent calls return the cached result.
///
/// # Returns
///
/// A reference to the cached [`SpiderTaskExecutorConfig`].
///
/// # Errors
///
/// Returns a reference to the cached error if the config failed to load.
pub fn spider_task_executor_config()
-> Result<&'static SpiderTaskExecutorConfig, &'static crate::Error> {
    SPIDER_TASK_EXECUTOR_CONFIG.as_ref()
}

/// The env var holding the path to the Spider task executor config YAML.
const CLP_CONFIG_PATH_ENV_VAR: &str = "CLP_CONFIG_PATH";

/// Process-wide cache of the Spider task executor config, populated on first access from
/// [`load_spider_task_executor_config_from_env`].
static SPIDER_TASK_EXECUTOR_CONFIG: LazyLock<Result<SpiderTaskExecutorConfig, crate::Error>> =
    LazyLock::new(load_spider_task_executor_config_from_env);

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
fn load_spider_task_executor_config_from_env() -> Result<SpiderTaskExecutorConfig, crate::Error> {
    let path = std::env::var(CLP_CONFIG_PATH_ENV_VAR)?;
    Ok(clp_rust_utils::serde::yaml::from_path(&path)?)
}
