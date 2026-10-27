//! [`QueryJobSubmitter`] implementation for [`spider_client::SpiderClient`].

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::time::Duration;

use async_trait::async_trait;
use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::task_io::query::ClpSQueryOption;
use clp_rust_utils::task_io::query::OutputHandle;
use clp_rust_utils::task_io::query::QueryTaskIndex;
use spider_client::SpiderClient;
use spider_client::error::ClientError;
use spider_core::job::JobState;
use spider_core::task::DataTypeDescriptor;
use spider_core::task::ExecutionPolicy;
use spider_core::task::TaskDescriptor;
use spider_core::task::TaskGraph;
use spider_core::task::TdlContext;
use spider_core::task::ValueTypeDescriptor;
use spider_core::types::id::JobId;
use spider_core::types::id::ResourceGroupId;
use spider_core::types::io::TaskGraphInput;
use spider_core::types::io::TaskGraphInputBuilder;

use crate::Error;
use crate::query_job_submitter::ArchiveMetadata;
use crate::query_job_submitter::QueryJobOutcome;
use crate::query_job_submitter::QueryJobSubmitter;

#[async_trait]
impl QueryJobSubmitter for SpiderClient {
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`build_query_task_graph`]'s return values on failure.
    /// * Forwards [`SpiderClient::submit_job`]'s return values on failure.
    async fn submit_query_job(
        &self,
        query_job_id: QueryJobId,
        resource_group_id: ResourceGroupId,
        clp_s_query_option: ClpSQueryOption,
        output_handle: OutputHandle,
        archives_to_search: Vec<(ArchiveMetadata, ExecutionPolicy)>,
    ) -> Result<JobId, Error> {
        let (graph, inputs) = build_query_task_graph(
            query_job_id,
            &clp_s_query_option,
            &output_handle,
            archives_to_search,
        )?;
        let spider_job_id = self.submit_job(resource_group_id, &graph, &inputs).await?;

        tracing::info!(
            query_job_id = % query_job_id,
            spider_job_id = % spider_job_id,
            num_tasks = graph.get_num_tasks(),
            "Submitted query job to Spider.",
        );

        Ok(spider_job_id)
    }

    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`SpiderClient::start_job`]'s return values on failure, except
    ///   [`ClientError::InvalidJobState`], which indicates the job has already been started.
    /// * Forwards [`SpiderClient::get_job_state`]'s return values on failure.
    async fn run_query_job_to_completion(
        &self,
        spider_job_id: JobId,
        poll_interval: Duration,
    ) -> Result<QueryJobOutcome, Error> {
        match self.start_job(spider_job_id).await {
            Ok(_) | Err(ClientError::InvalidJobState(_)) => {}
            Err(error) => return Err(error.into()),
        }

        let terminal_state = loop {
            let state = self.get_job_state(spider_job_id).await?;
            if state.is_terminal() {
                break state;
            }
            tokio::time::sleep(poll_interval).await;
        };

        Ok(match terminal_state {
            JobState::Succeeded => QueryJobOutcome::Succeeded,
            JobState::Failed => {
                let error_message = match self.get_job_error(spider_job_id).await {
                    Ok(error_message) => error_message,
                    Err(error) => {
                        tracing::warn!(
                            spider_job_id = % spider_job_id,
                            error = % error,
                            "Failed to fetch the Spider job error.",
                        );
                        format!("<failed to fetch job error: {error}>")
                    }
                };
                QueryJobOutcome::Failed { error_message }
            }
            JobState::Cancelled => QueryJobOutcome::Cancelled,
            _ => unreachable!("a terminal Spider state must have a terminal outcome"),
        })
    }
}

/// Builds independent archive-search tasks and their positionally ordered external inputs.
///
/// # Returns
///
/// A tuple on success, containing:
///
/// * The constructed task graph.
/// * The structured form of the task graph's input.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`TaskGraph::new`]'s return values on failure.
/// * Forwards [`ValueTypeDescriptor::struct_from_name`]'s return values on failure.
/// * Forwards [`TaskGraph::insert_task`]'s return values on failure.
/// * Forwards [`TaskGraphInputBuilder::create_shared_input_payload`]'s return values on failure.
/// * Forwards [`TaskGraphInputBuilder::append_shared_task_input`]'s return values on failure.
/// * Forwards [`TaskGraphInputBuilder::append_task_input`]'s return values on failure.
fn build_query_task_graph(
    query_job_id: QueryJobId,
    clp_s_query_option: &ClpSQueryOption,
    output_handle: &OutputHandle,
    archives_to_search: Vec<(ArchiveMetadata, ExecutionPolicy)>,
) -> Result<(TaskGraph, TaskGraphInput), Error> {
    // NOTE: Keep these names and the input order in sync with the TDL package definitions.
    const CLP_TDL_PACKAGE_NAME: &str = "clp";
    const QUERY_TASK_FUNC: &str = "query::clp_s_search";

    let mut graph = TaskGraph::new(None, None)?;

    let mut inputs = TaskGraphInputBuilder::new();
    let query_job_id_input = inputs.create_shared_input_payload(&query_job_id)?;
    let query_option_input = inputs.create_shared_input_payload(clp_s_query_option)?;
    let output_handle_input = inputs.create_shared_input_payload(output_handle)?;
    let mut dataset_inputs = HashMap::new();
    for (task_index, (archive, execution_policy)) in (0..).zip(archives_to_search) {
        graph.insert_task(TaskDescriptor {
            tdl_context: TdlContext {
                package: CLP_TDL_PACKAGE_NAME.to_owned(),
                task_func: QUERY_TASK_FUNC.to_owned(),
            },
            execution_policy: Some(execution_policy),
            inputs: vec![
                DataTypeDescriptor::Value(ValueTypeDescriptor::int32()),
                DataTypeDescriptor::Value(ValueTypeDescriptor::struct_from_name(
                    "ClpSQueryOption",
                )?),
                DataTypeDescriptor::Value(ValueTypeDescriptor::struct_from_name(
                    "Option<NonEmptyString>",
                )?),
                DataTypeDescriptor::Value(ValueTypeDescriptor::struct_from_name("NonEmptyString")?),
                DataTypeDescriptor::Value(ValueTypeDescriptor::struct_from_name("OutputHandle")?),
                // Spider has no unsigned integer type. A task index is below the number of tasks,
                // so it always fits in `int64`.
                DataTypeDescriptor::Value(ValueTypeDescriptor::int64()),
            ],
            outputs: vec![],
            input_sources: None,
        })?;
        inputs.append_shared_task_input(query_job_id_input)?;
        inputs.append_shared_task_input(query_option_input)?;
        let dataset_input = match dataset_inputs.entry(archive.dataset) {
            Entry::Occupied(entry) => *entry.get(),
            Entry::Vacant(entry) => {
                let input = inputs.create_shared_input_payload(entry.key())?;
                *entry.insert(input)
            }
        };
        inputs.append_shared_task_input(dataset_input)?;
        inputs.append_task_input(&archive.id)?;
        inputs.append_shared_task_input(output_handle_input)?;
        inputs.append_task_input::<QueryTaskIndex>(&task_index)?;
    }

    Ok((graph, inputs.build()))
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU16;

    use clp_rust_utils::task_io::query::ClpSQueryOption;
    use clp_rust_utils::task_io::query::OutputHandle;
    use clp_rust_utils::task_io::query::QueryTaskIndex;
    use clp_rust_utils::types::ArchiveId;
    use clp_rust_utils::types::non_empty_string::ExpectedNonEmpty;
    use non_empty_string::NonEmptyString;
    use spider_core::task::ExecutionPolicy;
    use spider_core::types::io::TaskGraphInputEntry;

    use super::build_query_task_graph;
    use crate::query_job_submitter::ArchiveMetadata;

    #[test]
    fn build_query_task_graph_appends_archive_positions_as_task_indices() -> anyhow::Result<()> {
        const ARCHIVE_IDS: [&str; 3] = [
            "018e90e5-8b2a-4a61-a2fc-cac799936caf",
            "5b0f8c2e-3d41-4a8e-9c7b-1e2f3a4b5c6d",
            "c3d2e1f0-a9b8-4c7d-8e6f-5a4b3c2d1e0f",
        ];

        let archives_to_search = ARCHIVE_IDS
            .iter()
            .zip([Some("ds1"), Some("ds2"), Some("ds1")])
            .map(|(archive_id, dataset)| {
                let archive = ArchiveMetadata {
                    id: archive_id.parse::<ArchiveId>()?,
                    dataset: dataset.map(NonEmptyString::from_static_str),
                    uncompressed_size: 0,
                    end_timestamp: 0,
                };
                Ok((archive, ExecutionPolicy::default()))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let clp_s_query_option = ClpSQueryOption {
            query_string: NonEmptyString::from_static_str("*Transmitted*"),
            max_num_results: None,
            begin_timestamp_millisecs: None,
            end_timestamp_millisecs: None,
            ignore_case: false,
        };
        let output_handle = OutputHandle::Network {
            host: NonEmptyString::from_static_str("10.0.0.7"),
            port: NonZeroU16::new(40_123).expect("40,123 is nonzero"),
            session_token: "6f1d3b52-8a4e-4c1b-9f6e-2d7a5c0b9e13".parse()?,
        };

        let (graph, inputs) =
            build_query_task_graph(42, &clp_s_query_option, &output_handle, archives_to_search)?;

        assert_eq!(graph.get_num_tasks(), ARCHIVE_IDS.len());
        let positional_inputs = inputs.get_positional_inputs();
        assert_eq!(
            positional_inputs.len(),
            graph.get_task_graph_input_indices().len()
        );
        let task_indices = positional_inputs
            .chunks(positional_inputs.len() / ARCHIVE_IDS.len())
            .map(|task_inputs| match task_inputs.last() {
                Some(TaskGraphInputEntry::ValuePayload(payload)) => {
                    Ok(rmp_serde::from_slice::<QueryTaskIndex>(payload)?)
                }
                entry => anyhow::bail!("unexpected task index input entry: {entry:?}"),
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        assert_eq!(task_indices, vec![0, 1, 2]);

        Ok(())
    }
}
