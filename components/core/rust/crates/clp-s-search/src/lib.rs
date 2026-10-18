//! The archive-bound query layer: column resolution against the schema tree, staged and DNF-free
//! planning (metadata, schema, dictionary and fold stages, with timestamp- and range-index
//! pruning and the `GrepCore` dictionary prefilter), projection resolution, and the vectorized,
//! polarity-aware kernels that evaluate residual filters over table windows.

mod archive;
mod bits;
mod error;
mod explain;
mod filter;
mod grep;
mod idset;
mod metadata;
mod outcome;
mod plan;
mod planner;
mod projection;
mod resolve;
mod telemetry;
mod tree;
mod wildcard;

pub use error::EvalError;
pub use error::PlanError;
pub use filter::CompiledTableFilter;
pub use filter::EvaluationScratch;
pub use grep::EncodedSubquery;
pub use grep::EncodedTextMatch;
pub use grep::EncodedVariableTest;
pub use idset::IdSet;
pub use plan::ArchiveSearchPlan;
pub use plan::BoundColumn;
pub use plan::ColumnTest;
pub use plan::EncodedTextMember;
pub use plan::LeafBinding;
pub use plan::RangeOutcomes;
pub use plan::ResidualFilter;
pub use plan::ResidualTemplate;
pub use plan::TableFilterPlan;
pub use plan::TableSearchPlan;
pub use planner::ArchivePlanner;
pub use planner::PlanLimits;
pub use planner::PlanStep;
pub use planner::SearchPlanner;
pub use projection::RecordProjection;
pub use resolve::ArrayAccess;
pub use telemetry::PruneReason;
pub use telemetry::PruneReport;
pub use telemetry::TerminationStage;

const _: () = {
    const fn assert_send_sync<SendSyncType: Send + Sync + 'static>() {}
    const fn assert_send<SendType: Send + 'static>() {}

    assert_send_sync::<SearchPlanner>();
    assert_send_sync::<ArchiveSearchPlan>();
    assert_send_sync::<CompiledTableFilter>();
    assert_send::<ArchivePlanner>();
    assert_send::<EvaluationScratch>();
};
