//! Handle for driving a single query job to completion.

use std::sync::Arc;

use clp_rust_utils::job_config::QueryJobConfig;
use clp_rust_utils::job_config::QueryJobId;
use spider_core::task::ExecutionPolicy;
use spider_core::types::id::JobId as SpiderJobId;
use spider_core::types::id::ResourceGroupId;

use crate::Error;
use crate::job_handle::JobHandle;
use crate::job_handle::JobHandleContext;
use crate::job_handle::archive::ArchiveMetadata;
use crate::job_handle::lifecycle::JobLifecycle;
use crate::job_handle::query::archive_selector::ArchiveSelector;
use crate::job_handle::query::task_graph;
use crate::spider_job_submitter::JobOutcome;
use crate::spider_job_submitter::SpiderJobSubmitter;

/// Handles the asynchronous submission of a query job and the retrieval of its result.
///
/// # Type Parameters
///
/// * `SubmitterType` - The type of the job submitter for Spider job submission.
pub struct QueryJobHandle<SubmitterType: SpiderJobSubmitter> {
    lifecycle: JobLifecycle<SubmitterType>,
    query_job_config: QueryJobConfig,
    archive_end_ts_lower_bound_millisecs: Option<i64>,
}

impl<SubmitterType: SpiderJobSubmitter> QueryJobHandle<SubmitterType> {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A newly created [`QueryJobHandle`] for the given query job.
    #[must_use]
    pub fn new(
        context: Arc<JobHandleContext>,
        query_job_id: QueryJobId,
        job_submitter: SubmitterType,
        resource_group_id: ResourceGroupId,
        query_job_config: QueryJobConfig,
        job_creation_timestamp_millisecs: i64,
    ) -> Self {
        let archive_end_ts_lower_bound_millisecs =
            context.archive_end_ts_lower_bound_millisecs(job_creation_timestamp_millisecs);

        Self {
            lifecycle: JobLifecycle::new(context, query_job_id, job_submitter, resource_group_id),
            query_job_config,
            archive_end_ts_lower_bound_millisecs,
        }
    }

    /// Plans the query job, then submits it to Spider and drives it to completion.
    ///
    /// If no archives are selected and no timeline is requested, it marks the query job as
    /// succeeded without submitting it.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Error::TooManyQueryTasks`] if the number of query tasks exceeds `i32`'s range.
    /// * Forwards [`Self::plan`]'s return values on failure.
    /// * Forwards [`JobLifecycle::terminate`]'s return values on failure for an empty plan.
    /// * Forwards [`task_graph::build`]'s return values on failure.
    /// * Forwards [`JobLifecycle::submit`]'s return values on failure.
    /// * Forwards [`JobLifecycle::to_completion`]'s return values on failure.
    async fn plan_and_run(&self) -> Result<(), Error> {
        let archives_to_search = self.plan().await?;
        if archives_to_search.is_empty() && !self.query_job_config.output.has_timeline() {
            return self.lifecycle.terminate(JobOutcome::Succeeded).await;
        }

        let num_tasks = archives_to_search.len();
        let persisted_num_tasks =
            i32::try_from(num_tasks).map_err(|_| Error::TooManyQueryTasks(num_tasks))?;
        let (graph, input) = task_graph::build(
            self.lifecycle.job_id(),
            &self.query_job_config.query,
            &self.query_job_config.output,
            archives_to_search,
        )?;
        let spider_job_id = self
            .lifecycle
            .submit(&graph, &input, persisted_num_tasks)
            .await?;
        self.lifecycle.to_completion(spider_job_id).await
    }

    /// Prepares the task inputs for the query job.
    ///
    /// # Returns
    ///
    /// A vector of tuples on success, where each tuple contains:
    ///
    /// * The [`ArchiveMetadata`] identifying the archive searched by a single query task.
    /// * The [`ExecutionPolicy`] for that task.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`ArchiveSelector::select`]'s return values on failure.
    async fn plan(&self) -> Result<Vec<(ArchiveMetadata, ExecutionPolicy)>, Error> {
        ArchiveSelector {
            context: self.lifecycle.context(),
            query_job_config: &self.query_job_config,
            archive_end_ts_lower_bound_millisecs: self.archive_end_ts_lower_bound_millisecs,
        }
        .select()
        .await
    }
}

impl<SubmitterType: SpiderJobSubmitter> JobHandle for QueryJobHandle<SubmitterType> {
    async fn run(self) -> Result<(), Error> {
        tracing::info!(job_id = % self.lifecycle.job_id(), "Starting query job.");

        self.lifecycle.run_to_terminal(self.plan_and_run()).await
    }

    async fn recover(self, spider_job_id: SpiderJobId) -> Result<(), Error> {
        self.lifecycle.recover(spider_job_id).await
    }
}
