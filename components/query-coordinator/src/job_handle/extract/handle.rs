//! Handle for driving a single extract job to completion.

use std::sync::Arc;

use clp_rust_utils::job_config::ExtractJobConfig;
use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::task_io::extraction::ExtractTaskInput;
use spider_core::task::ExecutionPolicy;
use spider_core::types::id::JobId as SpiderJobId;
use spider_core::types::id::ResourceGroupId;

use crate::Error;
use crate::job_handle::JobHandle;
use crate::job_handle::JobHandleContext;
use crate::job_handle::archive::EXTRACT_TIMEOUT_RATES;
use crate::job_handle::extract::task_graph;
use crate::job_handle::lifecycle::JobLifecycle;
use crate::spider_job_submitter::SpiderJobSubmitter;

/// Handles the asynchronous submission of an extract job and the retrieval of its result.
///
/// # Type Parameters
///
/// * `SubmitterType` - The type of the job submitter for Spider job submission.
pub struct ExtractJobHandle<SubmitterType: SpiderJobSubmitter> {
    lifecycle: JobLifecycle<SubmitterType>,
    task_input: ExtractTaskInput,
    archive_end_ts_lower_bound_millisecs: Option<i64>,
}

impl<SubmitterType: SpiderJobSubmitter> ExtractJobHandle<SubmitterType> {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A newly created [`ExtractJobHandle`] for the given extract job.
    #[must_use]
    pub fn new(
        context: Arc<JobHandleContext>,
        job_id: QueryJobId,
        job_submitter: SubmitterType,
        resource_group_id: ResourceGroupId,
        extract_job_config: ExtractJobConfig,
        job_creation_timestamp_millisecs: i64,
    ) -> Self {
        let archive_end_ts_lower_bound_millisecs =
            context.archive_end_ts_lower_bound_millisecs(job_creation_timestamp_millisecs);
        let ExtractJobConfig {
            dataset,
            archive_id,
            target_chunk_size,
        } = extract_job_config;

        Self {
            lifecycle: JobLifecycle::new(context, job_id, job_submitter, resource_group_id),
            task_input: ExtractTaskInput {
                job_id,
                dataset,
                archive_id,
                target_chunk_size,
            },
            archive_end_ts_lower_bound_millisecs,
        }
    }

    /// Plans the extract job, then submits its single extraction task to Spider and drives it to
    /// completion.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::plan`]'s return values on failure.
    /// * Forwards [`task_graph::build`]'s return values on failure.
    /// * Forwards [`JobLifecycle::submit`]'s return values on failure.
    /// * Forwards [`JobLifecycle::to_completion`]'s return values on failure.
    async fn plan_and_run(&self) -> Result<(), Error> {
        let execution_policy = self.plan().await?;
        let (graph, input) = task_graph::build(&self.task_input, execution_policy)?;
        let spider_job_id = self.lifecycle.submit(&graph, &input, 1).await?;
        self.lifecycle.to_completion(spider_job_id).await
    }

    /// Checks that the archive to extract exists and is within the archive retention period.
    ///
    /// # Returns
    ///
    /// The execution policy of the extraction task, derived from the archive's uncompressed size,
    /// on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Error::InvalidJobConfig`] if:
    ///   * The archive doesn't exist in the dataset.
    ///   * The archive ends before the job's archive retention lower bound.
    /// * Forwards [`JobHandleContext::ensure_datasets_exist`]'s return values on failure.
    /// * Forwards [`JobHandleContext::fetch_archive`]'s return values on failure.
    async fn plan(&self) -> Result<ExecutionPolicy, Error> {
        let context = self.lifecycle.context();
        let ExtractTaskInput {
            dataset,
            archive_id,
            ..
        } = &self.task_input;

        context
            .ensure_datasets_exist(std::iter::once(dataset))
            .await?;
        let archive = context
            .fetch_archive(dataset, *archive_id)
            .await?
            .ok_or_else(|| {
                Error::InvalidJobConfig(format!(
                    "archive {archive_id} doesn't exist in dataset `{dataset}`"
                ))
            })?;
        if !archive.is_within_retention(self.archive_end_ts_lower_bound_millisecs) {
            return Err(Error::InvalidJobConfig(format!(
                "archive {archive_id} is outside the archive retention period"
            )));
        }

        Ok(EXTRACT_TIMEOUT_RATES.task_execution_policy(
            archive.uncompressed_size,
            context.spider_option.query_task_max_retry,
        ))
    }
}

impl<SubmitterType: SpiderJobSubmitter> JobHandle for ExtractJobHandle<SubmitterType> {
    async fn run(self) -> Result<(), Error> {
        tracing::info!(
            job_id = % self.lifecycle.job_id(),
            dataset = % self.task_input.dataset,
            archive_id = % self.task_input.archive_id,
            "Starting extract job.",
        );

        let result = self.plan_and_run().await;
        if let Err(error) = &result {
            self.lifecycle.finalize_on_error(error).await;
        }
        result
    }

    async fn recover(self, spider_job_id: SpiderJobId) -> Result<(), Error> {
        self.lifecycle.recover(spider_job_id).await
    }
}
