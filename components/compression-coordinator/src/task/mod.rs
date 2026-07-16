//! Compression task implementations and the worker-level config they read at runtime.

use std::sync::LazyLock;

use clp_rust_utils::clp_config::package::config::WorkerConfig;

pub mod s3_compression;

/// Returns the process-wide worker config, loading it from `CLP_CONFIG_PATH` on first access.
///
/// The config is loaded once, on first access; subsequent calls return the cached result.
///
/// # Returns
///
/// A reference to the cached [`WorkerConfig`].
///
/// # Errors
///
/// Returns a reference to the cached error if the config failed to load.
pub fn worker_config() -> Result<&'static WorkerConfig, &'static crate::Error> {
    WORKER_CONFIG.as_ref()
}

/// The env var holding the path to the worker-config YAML.
const CLP_CONFIG_PATH_ENV_VAR: &str = "CLP_CONFIG_PATH";

/// Process-wide cache of the worker config, populated on first access from
/// [`load_worker_config_from_env`].
static WORKER_CONFIG: LazyLock<Result<WorkerConfig, crate::Error>> =
    LazyLock::new(load_worker_config_from_env);

/// Loads the [`WorkerConfig`] from the YAML file at the path named by [`CLP_CONFIG_PATH_ENV_VAR`].
///
/// # Returns
///
/// The deserialized [`WorkerConfig`].
///
/// # Errors
///
/// Returns an error if [`CLP_CONFIG_PATH_ENV_VAR`] is unset or not valid Unicode, or if the YAML
/// file at that path cannot be read or parsed.
fn load_worker_config_from_env() -> Result<WorkerConfig, crate::Error> {
    let path = std::env::var(CLP_CONFIG_PATH_ENV_VAR)?;
    Ok(clp_rust_utils::serde::yaml::from_path(&path)?)
}
