//! The query tasks: the `#[task]` wrappers Spider invokes and their implementations.

use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::task_io::query::ClpSQueryOption;
use clp_rust_utils::task_io::query::OutputHandle;
use clp_rust_utils::task_io::query::QueryTaskIndex;
use clp_rust_utils::types::ArchiveId;
use non_empty_string::NonEmptyString;
use spider_tdl::TaskContext;
use spider_tdl::TdlError;
use spider_tdl::task;

mod search;

#[task(name = "query::clp_s_search")]
pub(crate) fn clp_s_search_task(
    ctx: TaskContext,
    query_job_id: QueryJobId,
    clp_s_query_option: ClpSQueryOption,
    dataset: Option<NonEmptyString>,
    archive_id: ArchiveId,
    output_handle: OutputHandle,
    task_index: QueryTaskIndex,
) -> Result<(), TdlError> {
    search::search(
        &ctx,
        crate::common::spider_task_executor_config(),
        query_job_id,
        &clp_s_query_option,
        &search::ArchiveToSearch {
            id: archive_id,
            dataset,
            task_index,
        },
        &output_handle,
    )
    .map_err(|e| TdlError::ExecutionError(format!("{e:#}")))
}
