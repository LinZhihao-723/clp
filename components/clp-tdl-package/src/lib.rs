//! Spider TDL task package `clp`: wrappers around the compression-coordinator worker fns.

use compression_coordinator::{
    ClpSCompressionOption,
    CompressionTaskOutput,
    S3InputSource,
    commit,
    compress,
    init_config,
    init_runtime,
    spider_task_executor_config,
};
use spider_tdl::{TaskContext, TdlError, task};

/// Installs a stderr `tracing` subscriber into this cdylib's own global dispatcher.
///
/// The task-executor and this dlopen'd package each statically link their own copy of `tracing`,
/// so they have independent global dispatchers. The executor's subscriber is invisible here;
/// without this call every task-side event hits `NoSubscriber` and is dropped. The subscriber
/// mirrors the executor's format (JSON to stderr, `RUST_LOG`-driven filter) so both streams
/// interleave in the same executor log file.
///
/// # Errors
///
/// Returns an error if a global subscriber is already installed for this dispatcher.
fn init_stderr_tracing_subscriber() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

/// Initializes this package's process-global state (Tokio runtime, worker config, tracing
/// subscriber) at package load.
///
/// # Errors
///
/// Returns an error if the Tokio runtime fails to build, the worker config cannot be loaded, or the
/// tracing subscriber cannot be installed.
fn package_init() -> Result<(), TdlError> {
    init_runtime().map_err(|e| TdlError::ExecutionError(format!("{e:#}")))?;
    init_config().map_err(|e| TdlError::ExecutionError(format!("{e:#}")))?;
    init_stderr_tracing_subscriber().map_err(|e| {
        TdlError::ExecutionError(format!("failed to install the tracing subscriber: {e}"))
    })?;
    Ok(())
}

#[task(name = "compression::clp_s_compress")]
fn compress_task(
    ctx: TaskContext,
    clp_s_option: ClpSCompressionOption,
    dataset: Option<String>,
    input_source: S3InputSource,
) -> Result<CompressionTaskOutput, TdlError> {
    tracing::info!(
        job_id = %ctx.job_id,
        task_id = %ctx.task_id,
        task_instance_id = ctx.task_instance_id,
        dataset = dataset.as_deref().unwrap_or("<default>"),
        "CLP compression task started."
    );
    compress(
        &ctx,
        spider_task_executor_config(),
        &clp_s_option,
        dataset,
        input_source,
    )
    .map_err(|e| TdlError::ExecutionError(format!("{e:#}")))
}

#[task(name = "compression::clp_s_commit")]
fn commit_task(ctx: TaskContext) -> Result<(), TdlError> {
    tracing::info!(job_id = %ctx.job_id, "CLP commit task started.");
    let outputs = ctx
        .get_task_graph_outputs()?
        .ok_or_else(|| TdlError::ExecutionError("commit must run as a commit task".to_owned()))?;
    let outputs: Vec<CompressionTaskOutput> = outputs
        .iter()
        .map(|blob| rmp_serde::from_slice(blob))
        .collect::<Result<_, _>>()
        .map_err(|e| TdlError::DeserializationError(e.to_string()))?;
    let dataset = outputs.first().and_then(|output| output.dataset.clone());
    let archives = outputs
        .into_iter()
        .flat_map(|output| output.archives)
        .collect();
    commit(ctx.job_id, dataset, archives).map_err(|e| TdlError::ExecutionError(format!("{e:#}")))
}

spider_tdl::register_tdl_package! {
    package_name: "clp",
    init: package_init,
    tasks: [compress_task, commit_task],
}
