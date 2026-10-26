//! The query tasks: the `#[task]` wrappers Spider invokes and their implementations.

use anyhow::Context;
use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::task_io::query::ArchiveToSearch;
use clp_rust_utils::task_io::query::ClpSQueryOption;
use clp_rust_utils::task_io::query::OutputHandle;
use non_empty_string::NonEmptyString;
use spider_tdl::TaskContext;
use spider_tdl::TdlError;
use spider_tdl::task;

use crate::common::SearchEngine;

mod search;

#[task(name = "query::clp_s_search")]
pub(crate) fn clp_s_search_task(
    ctx: TaskContext,
    query_job_id: QueryJobId,
    clp_s_query_option: ClpSQueryOption,
    output_handle: OutputHandle,
    archives: Vec<ArchiveToSearch>,
) -> Result<(), TdlError> {
    let config = crate::common::spider_task_executor_config();
    let search_engine = crate::common::search_engine();
    archives
        .iter()
        .try_for_each(|archive| {
            match search_engine {
                SearchEngine::ClpS => search::search(
                    &ctx,
                    config,
                    query_job_id,
                    &clp_s_query_option,
                    archive,
                    &output_handle,
                ),
                SearchEngine::Rust => clp_s_spider_adapter::search(
                    &ctx,
                    config,
                    query_job_id,
                    &clp_s_query_option,
                    clp_s_spider_adapter::ArchiveToSearch {
                        id: archive.id,
                        dataset: archive.dataset.as_ref().map(NonEmptyString::as_str),
                        task_index: archive.task_index,
                    },
                    &output_handle,
                ),
            }
            .with_context(|| format!("failed to search archive {}", archive.id))
        })
        .map_err(|e| TdlError::ExecutionError(format!("{e:#}")))
}
