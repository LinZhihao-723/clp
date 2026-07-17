//! Spider TDL task package `clp`: wrappers around the compression-coordinator worker fns.

use compression_coordinator::{
    ClpSCompressionOption,
    CompressionTaskOutput,
    S3InputSource,
    commit,
    compress,
    spider_task_executor_config,
};
use spider_tdl::{TaskContext, TdlError, task};

#[task(name = "compression::clp_s_compress")]
fn compress_task(
    ctx: TaskContext,
    clp_s_option: ClpSCompressionOption,
    dataset: Option<String>,
    input_source: S3InputSource,
) -> Result<CompressionTaskOutput, TdlError> {
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
