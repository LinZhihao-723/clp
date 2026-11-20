//! The Spider task graph of an extract job.

use clp_rust_utils::task_io::extraction::ExtractTaskInput;
use spider_core::task::DataTypeDescriptor;
use spider_core::task::ExecutionPolicy;
use spider_core::task::TaskDescriptor;
use spider_core::task::TaskGraph;
use spider_core::task::TdlContext;
use spider_core::task::ValueTypeDescriptor;
use spider_core::types::io::TaskGraphInput;
use spider_core::types::io::TaskGraphInputBuilder;

use crate::Error;

/// Builds a graph of a single task that extracts an archive's JSON stream as described by
/// `task_input`.
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
/// * Forwards [`TaskGraphInputBuilder::append_task_input`]'s return values on failure.
pub(super) fn build(
    task_input: &ExtractTaskInput,
    execution_policy: ExecutionPolicy,
) -> Result<(TaskGraph, TaskGraphInput), Error> {
    // NOTE: Keep these names and the input type in sync with the TDL package definitions.
    const CLP_TDL_PACKAGE_NAME: &str = "clp";
    const EXTRACT_TASK_FUNC: &str = "query::clp_s_extract_json";
    const EXTRACT_TASK_INPUT_TYPE_NAME: &str = "ExtractTaskInput";

    let mut graph = TaskGraph::new(None, None)?;
    graph.insert_task(TaskDescriptor {
        tdl_context: TdlContext {
            package: CLP_TDL_PACKAGE_NAME.to_owned(),
            task_func: EXTRACT_TASK_FUNC.to_owned(),
        },
        execution_policy: Some(execution_policy),
        inputs: vec![DataTypeDescriptor::Value(
            ValueTypeDescriptor::struct_from_name(EXTRACT_TASK_INPUT_TYPE_NAME)?,
        )],
        outputs: vec![],
        input_sources: None,
    })?;

    let mut input = TaskGraphInputBuilder::new();
    input.append_task_input(task_input)?;
    Ok((graph, input.build()))
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use clp_rust_utils::task_io::extraction::ExtractTaskInput;
    use spider_core::task::DataTypeDescriptor;
    use spider_core::task::ExecutionPolicy;
    use spider_core::task::TaskInputOutputIndex;
    use spider_core::task::TimeoutPolicy;
    use spider_core::task::ValueTypeDescriptor;
    use spider_core::types::io::TaskGraphInputEntry;

    use super::build;

    #[test]
    fn graph_is_one_extraction_task_with_the_task_input_as_its_input() -> anyhow::Result<()> {
        let task_input = ExtractTaskInput {
            job_id: 7,
            dataset: "logs".parse()?,
            archive_id: "018e90e5-8b2a-4a61-a2fc-cac799936caf".parse()?,
            target_chunk_size: NonZeroU64::new(1024),
        };
        let execution_policy = ExecutionPolicy {
            max_num_retry: 5,
            max_num_instances: 1,
            timeout_policy: TimeoutPolicy {
                soft_timeout_ms: 1000,
                hard_timeout_ms: 10_000,
            },
        };

        let (graph, input) = build(&task_input, execution_policy.clone())?;

        assert_eq!(graph.get_num_tasks(), 1);
        assert_eq!(graph.get_commit_task_descriptor(), None);
        assert_eq!(graph.get_cleanup_task_descriptor(), None);
        let task = graph.get_task(0).expect("the graph contains one task");
        assert_eq!(task.get_tdl_context().package, "clp");
        assert_eq!(
            task.get_tdl_context().task_func,
            "query::clp_s_extract_json"
        );
        assert_eq!(task.get_execution_policy(), &execution_policy);
        assert_eq!(task.get_input_dep_indices().as_slice(), &[0]);
        assert_eq!(
            graph
                .get_task_input(TaskInputOutputIndex {
                    task_idx: 0,
                    position: 0,
                })
                .expect("the task has one input")
                .get_type_descriptor(),
            &DataTypeDescriptor::Value(ValueTypeDescriptor::struct_from_name("ExtractTaskInput")?)
        );

        let [TaskGraphInputEntry::ValuePayload(payload)] = input.get_positional_inputs() else {
            panic!("the graph input should be one value payload");
        };
        assert_eq!(
            rmp_serde::from_slice::<ExtractTaskInput>(payload)?,
            task_input
        );
        Ok(())
    }
}
