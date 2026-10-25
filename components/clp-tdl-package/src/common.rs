//! Process-global state shared by this package's tasks: the Tokio runtime, the Spider task
//! executor config, the search engine with its in-process search resources, and this cdylib's
//! `tracing` subscriber.

use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::Context;
use clp_rust_utils::clp_config::package::config::SpiderTaskExecutorConfig;

/// The engine that the query tasks search archives with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchEngine {
    /// The C++ `clp-s` binary, spawned per task.
    ClpS,

    /// The Rust clp-s reader library, in-process.
    Rust,
}

impl SearchEngine {
    /// Factory function.
    ///
    /// Parses the value of [`SEARCH_ENGINE_ENV_VAR`]: unset or `clp-s` selects [`Self::ClpS`],
    /// and `rust` selects [`Self::Rust`].
    ///
    /// # Returns
    ///
    /// The selected engine on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`anyhow::Error`] if `value` names no engine.
    fn from_env_value(value: Option<&str>) -> anyhow::Result<Self> {
        match value {
            None | Some("clp-s") => Ok(Self::ClpS),
            Some("rust") => Ok(Self::Rust),
            Some(value) => anyhow::bail!(
                "unknown search engine `{value}` in `{SEARCH_ENGINE_ENV_VAR}`; expected `clp-s` \
                 or `rust`"
            ),
        }
    }
}

/// Initializes the process-wide Tokio runtime the compression tasks use to drive async S3 I/O.
///
/// Idempotent: a no-op once the runtime is initialized.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forward [`tokio::runtime::Builder::build`]'s return values on failure.
pub fn init_runtime() -> anyhow::Result<()> {
    if TOKIO_RUNTIME.get().is_some() {
        return Ok(());
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build the clp-tdl-package Tokio runtime")?;
    let _ = TOKIO_RUNTIME.set(rt);
    Ok(())
}

/// Initializes the process-wide Spider task executor config from `CLP_CONFIG_PATH`.
///
/// Idempotent: a no-op once the config is initialized.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`load_spider_task_executor_config_from_env`]'s return values on failure.
pub fn init_config() -> anyhow::Result<()> {
    if SPIDER_TASK_EXECUTOR_CONFIG.get().is_some() {
        return Ok(());
    }
    let cfg = load_spider_task_executor_config_from_env()?;
    let _ = SPIDER_TASK_EXECUTOR_CONFIG.set(cfg);
    Ok(())
}

/// Initializes the process-wide CLP home directory from `CLP_HOME`.
///
/// Idempotent: a no-op once the directory is initialized.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`std::env::var`]'s return values on failure.
pub fn init_clp_home() -> anyhow::Result<()> {
    if CLP_HOME.get().is_some() {
        return Ok(());
    }
    let clp_home = std::env::var(CLP_HOME_ENV_VAR)
        .with_context(|| format!("failed to read the `{CLP_HOME_ENV_VAR}` environment variable"))?;
    let _ = CLP_HOME.set(PathBuf::from(clp_home));
    Ok(())
}

/// Initializes the process-wide search engine from `CLP_SPIDER_SEARCH_ENGINE`.
///
/// Idempotent: a no-op once the engine is initialized.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if the env var is set but not valid Unicode.
/// * Forwards [`SearchEngine::from_env_value`]'s return values on failure.
pub fn init_search_engine() -> anyhow::Result<()> {
    if SEARCH_ENGINE.get().is_some() {
        return Ok(());
    }
    let value = match std::env::var(SEARCH_ENGINE_ENV_VAR) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(e) => {
            return Err(e).with_context(|| {
                format!("failed to read the `{SEARCH_ENGINE_ENV_VAR}` environment variable")
            });
        }
    };
    let _ = SEARCH_ENGINE.set(SearchEngine::from_env_value(value.as_deref())?);
    Ok(())
}

/// Installs the process-wide resources of the in-process search if the search engine is
/// [`SearchEngine::Rust`]; a no-op otherwise.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`clp_s_reader::ReaderContextBuilder::build`]'s return values on failure.
/// * Forwards [`clp_s_spider_adapter::AdapterResources::init`]'s return values on failure.
///
/// # Panics
///
/// Panics if the search engine, the runtime or the CLP home has not been initialized.
pub fn init_rust_search_resources() -> anyhow::Result<()> {
    /// How long an in-process search waits for the memory budget to admit a stream.
    const BASE_ADMISSION_TIMEOUT_SEC: u64 = 60;

    if SearchEngine::Rust != search_engine() {
        return Ok(());
    }
    clp_s_spider_adapter::AdapterResources::init(
        runtime(),
        clp_s_reader::ReaderContext::builder().build()?,
        clp_s_spider_adapter::AdapterConfig {
            clp_home: clp_home().to_path_buf(),
            base_admission_timeout_sec: BASE_ADMISSION_TIMEOUT_SEC,
        },
    )
}

/// Installs `tracing` subscriber into the cdylib's own global dispatcher writing to `stderr`.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`tracing_subscriber::fmt::SubscriberBuilder::try_init`]'s return values on failure.
pub fn init_stderr_tracing_subscriber() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .event_format(
            tracing_subscriber::fmt::format()
                .with_level(true)
                .with_target(false)
                .with_file(true)
                .with_line_number(true)
                .json(),
        )
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .try_init()
}

/// # Returns
///
/// A reference to the cached executor [`SpiderTaskExecutorConfig`].
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

/// # Returns
///
/// A reference to the cached CLP home directory.
///
/// # Panics
///
/// Panics if the directory has not been initialized by the TDL package init hook.
#[must_use]
pub fn clp_home() -> &'static Path {
    CLP_HOME
        .get()
        .expect("CLP home not initialized; the TDL package init hook must run before any task")
}

/// # Returns
///
/// The process-wide search engine.
///
/// # Panics
///
/// Panics if the engine has not been initialized by the TDL package init hook.
#[must_use]
pub fn search_engine() -> SearchEngine {
    *SEARCH_ENGINE
        .get()
        .expect("search engine not initialized; the TDL package init hook must run before any task")
}

/// # Returns
///
/// A handle to the process-wide multithreaded Tokio runtime.
///
/// # Panics
///
/// Panics if the runtime has not been initialized by the TDL package init hook.
#[must_use]
pub fn runtime() -> tokio::runtime::Handle {
    TOKIO_RUNTIME
        .get()
        .expect("Tokio runtime not initialized; the TDL package init hook must run before any task")
        .handle()
        .clone()
}

/// The env var holding the path to the Spider task executor config YAML.
const CLP_CONFIG_PATH_ENV_VAR: &str = "CLP_CONFIG_PATH";

/// The env var holding the path to the CLP installation's home directory.
const CLP_HOME_ENV_VAR: &str = "CLP_HOME";

/// The env var selecting the engine that the query tasks search archives with.
const SEARCH_ENGINE_ENV_VAR: &str = "CLP_SPIDER_SEARCH_ENGINE";

/// Process-wide CLP home directory, initialized by [`init_clp_home`].
static CLP_HOME: OnceLock<PathBuf> = OnceLock::new();

/// Process-wide search engine, initialized by [`init_search_engine`].
static SEARCH_ENGINE: OnceLock<SearchEngine> = OnceLock::new();

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
/// Returns an error if:
///
/// * Forwards [`std::env::var`]'s return values on failure if failed to read the env var.
/// * Forwards [`clp_rust_utils::serde::yaml::from_path`]'s return values on failure if failed to
///   load the executor config.
fn load_spider_task_executor_config_from_env() -> anyhow::Result<SpiderTaskExecutorConfig> {
    let path = std::env::var(CLP_CONFIG_PATH_ENV_VAR).with_context(|| {
        format!("failed to read the `{CLP_CONFIG_PATH_ENV_VAR}` environment variable")
    })?;
    clp_rust_utils::serde::yaml::from_path(&path)
        .with_context(|| format!("failed to load the Spider task executor config from `{path}`"))
}

#[cfg(test)]
mod tests {
    use super::SearchEngine;

    #[test]
    fn search_engines_are_parsed_from_the_env_value() {
        assert_eq!(
            SearchEngine::from_env_value(None).ok(),
            Some(SearchEngine::ClpS)
        );
        assert_eq!(
            SearchEngine::from_env_value(Some("clp-s")).ok(),
            Some(SearchEngine::ClpS)
        );
        assert_eq!(
            SearchEngine::from_env_value(Some("rust")).ok(),
            Some(SearchEngine::Rust)
        );
        for value in ["", "Rust", "clp_s", "cpp"] {
            let error = SearchEngine::from_env_value(Some(value))
                .expect_err("only `clp-s` and `rust` name an engine");
            assert!(
                error.to_string().contains("unknown search engine"),
                "{error}"
            );
        }
    }
}
