//! Spider TDL task package `clp`: wrappers around the compression-coordinator worker fns.

use std::sync::Once;

use compression_coordinator::{
    ClpSCompressionOption,
    CompressionTaskOutput,
    S3InputSource,
    commit,
    compress,
    spider_task_executor_config,
};
use spider_tdl::{TaskContext, TdlError, task};

/// Guards one-time installation of this package's tracing subscriber.
static INIT_TASK_TRACING: Once = Once::new();

/// Installs a stderr `tracing` subscriber into this cdylib's own global dispatcher.
///
/// The task-executor and this dlopen'd package each statically link their own copy of `tracing`,
/// so they have independent global dispatchers. The executor's subscriber is invisible here; without
/// this call every task-side event hits `NoSubscriber` and is dropped. The subscriber mirrors the
/// executor's format (JSON to stderr, `RUST_LOG`-driven filter) so both streams interleave in the
/// same executor log file.
fn init_task_tracing() {
    INIT_TASK_TRACING.call_once(|| {
        let _ = tracing_subscriber::fmt()
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
            .try_init();
    });
}

#[task(name = "compression::clp_s_compress")]
fn compress_task(
    ctx: TaskContext,
    clp_s_option: ClpSCompressionOption,
    dataset: Option<String>,
    input_source: S3InputSource,
) -> Result<CompressionTaskOutput, TdlError> {
    init_task_tracing();
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
    init_task_tracing();
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
    tasks: [compress_task, commit_task],
}
