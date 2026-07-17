//! [`S3CompressionJobSubmitter`] implementation for [`spider_client::SpiderClient`].

use std::time::Duration;

use async_trait::async_trait;
use spider_client::{SpiderClient, error::ClientError};
use spider_core::{
    job::JobState,
    task::{
        DataTypeDescriptor,
        TaskDescriptor,
        TaskGraph,
        TdlContext,
        TerminationTaskDescriptor,
        ValueTypeDescriptor,
    },
    types::{
        id::{JobId, ResourceGroupId},
        io::TaskInput,
    },
};

use crate::{
    compression_job_submitter::{CompressionJobCompletion, S3CompressionJobSubmitter},
    error::Error,
    task_io::{ClpSCompressionOption, S3InputSource},
};

/// The TDL package that registers the compression and commit tasks.
///
/// These three constants MUST stay in sync with `clp-tdl-package`'s `register_tdl_package!`
/// `package_name` and `#[task(name = ...)]` attributes. A direct dependency on that crate would
/// form a cycle, so the identifiers are duplicated here on purpose.
const CLP_TDL_PACKAGE_NAME: &str = "clp";

/// The compression task's TDL function name.
const COMPRESSION_TASK_FUNC: &str = "compression::clp_s_compress";

/// The commit (termination) task's TDL function name.
const COMMIT_TASK_FUNC: &str = "compression::clp_s_commit";

/// The number of positional inputs the compression task takes:
/// `(clp_s_option, dataset, input_source)`.
const COMPRESSION_TASK_NUM_INPUTS: usize = 3;

/// The initial delay between job-state polls while waiting for a job to reach a terminal state.
const INITIAL_POLL_BACKOFF: Duration = Duration::from_millis(500);

/// The maximum delay between job-state polls while waiting for a job to reach a terminal state.
const MAX_POLL_BACKOFF: Duration = Duration::from_secs(5);

/// The multiplier applied to the poll backoff after each non-terminal poll.
const POLL_BACKOFF_FACTOR: u32 = 2;

#[async_trait]
impl S3CompressionJobSubmitter for SpiderClient {
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Error::TaskGraph`] if building the task graph or inserting a compression task fails.
    /// * [`Error::TaskInputSerialization`] if a task input cannot be msgpack-serialized.
    /// * [`Error::Cluster`] if the Spider cluster rejects the job submission.
    async fn submit_s3_compression_job(
        &self,
        resource_group_id: ResourceGroupId,
        clp_s_option: ClpSCompressionOption,
        dataset: Option<String>,
        input_sources: Vec<S3InputSource>,
    ) -> Result<JobId, Error> {
        let bytes_type = DataTypeDescriptor::Value(ValueTypeDescriptor::bytes());
        let commit_task = TerminationTaskDescriptor {
            tdl_context: TdlContext {
                package: CLP_TDL_PACKAGE_NAME.to_owned(),
                task_func: COMMIT_TASK_FUNC.to_owned(),
            },
            execution_policy: None,
        };
        let mut graph = TaskGraph::new(Some(commit_task), None)?;

        let mut inputs: Vec<TaskInput> =
            Vec::with_capacity(input_sources.len() * COMPRESSION_TASK_NUM_INPUTS);
        for input_source in input_sources {
            graph.insert_task(TaskDescriptor {
                tdl_context: TdlContext {
                    package: CLP_TDL_PACKAGE_NAME.to_owned(),
                    task_func: COMPRESSION_TASK_FUNC.to_owned(),
                },
                execution_policy: None,
                inputs: vec![bytes_type.clone(); COMPRESSION_TASK_NUM_INPUTS],
                outputs: vec![bytes_type.clone()],
                input_sources: None,
            })?;
            inputs.push(TaskInput::ValuePayload(rmp_serde::to_vec(&clp_s_option)?));
            inputs.push(TaskInput::ValuePayload(rmp_serde::to_vec(&dataset)?));
            inputs.push(TaskInput::ValuePayload(rmp_serde::to_vec(&input_source)?));
        }

        self.submit_job(resource_group_id, &graph, inputs)
            .await
            .map_err(|error| Error::Cluster(error.to_string()))
    }

    /// Polls the job's state until it reaches a terminal state, sleeping between polls with a
    /// capped exponential backoff.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Error::Cluster`] if starting the job (for a reason other than it already having been
    ///   started) or polling its state fails on the Spider cluster.
    async fn run_s3_compression_job_to_completion(
        &self,
        spider_job_id: JobId,
    ) -> Result<CompressionJobCompletion, Error> {
        match self.start_job(spider_job_id).await {
            Ok(_) | Err(ClientError::InvalidJobState(_)) => {}
            Err(error) => return Err(Error::Cluster(error.to_string())),
        }

        let mut backoff = INITIAL_POLL_BACKOFF;
        let terminal_state = loop {
            let state = self
                .get_job_state(spider_job_id)
                .await
                .map_err(|error| Error::Cluster(error.to_string()))?;
            if state.is_terminal() {
                break state;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * POLL_BACKOFF_FACTOR).min(MAX_POLL_BACKOFF);
        };

        Ok(match terminal_state {
            JobState::Succeeded => CompressionJobCompletion::Succeeded,
            JobState::Failed => CompressionJobCompletion::Failed {
                error_message: self
                    .get_job_error(spider_job_id)
                    .await
                    .unwrap_or_else(|error| format!("<failed to fetch job error: {error}>")),
            },
            JobState::Cancelled => CompressionJobCompletion::Cancelled,
            _ => unreachable!(),
        })
    }
}
