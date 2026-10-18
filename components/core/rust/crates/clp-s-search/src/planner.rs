//! The staged, I/O-free planner.

use std::sync::Arc;

use clp_s_format::ArchiveMetadata;
use clp_s_format::DictionaryNeeds;
use clp_s_query::ColumnRef;
use clp_s_query::NormalExpr;
use clp_s_query::Query;
use clp_s_table::ArchiveTables;

use crate::ArchiveSearchPlan;
use crate::PlanError;
use crate::PruneReason;
use crate::PruneReport;
use crate::RecordProjection;
use crate::TerminationStage;
use crate::archive::SchemaStage;
use crate::archive::SchemaStageOutcome;
use crate::explain;
use crate::metadata::authoritative_timestamp_outcomes;
use crate::metadata::range_index_outcomes;
use crate::outcome::OutcomeSet;
use crate::tree;
use crate::tree::FoldNode;

/// Bounds on plan memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanLimits {
    /// Maximum bytes of id sets and bindings one archive plan may hold.
    pub max_plan_size: u64,
}

impl Default for PlanLimits {
    fn default() -> Self {
        Self {
            max_plan_size: 256 * 1024 * 1024,
        }
    }
}

/// Plans one query against any number of archives; `Send + Sync`, does no I/O.
#[derive(Debug)]
pub struct SearchPlanner {
    query: Arc<Query>,
    projection: RecordProjection,
    limits: PlanLimits,
}

impl SearchPlanner {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The planner on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`PlanError::InvalidProjection`] if a projection column has a wildcard.
    pub fn new(
        query: Arc<Query>,
        projection: &RecordProjection,
        limits: PlanLimits,
    ) -> Result<Self, PlanError> {
        if let RecordProjection::Columns(columns) = projection
            && let Some(column) = columns.iter().find(|column| !column.is_literal())
        {
            return Err(PlanError::InvalidProjection {
                column: explain::column_text(column).into(),
                reason: "projection columns cannot contain wildcards",
            });
        }
        Ok(Self {
            query,
            projection: projection.clone(),
            limits,
        })
    }

    /// # Returns
    ///
    /// The query being planned.
    #[must_use]
    pub const fn query(&self) -> &Arc<Query> {
        &self.query
    }

    /// Stage A: evaluates the query's time leaves against the archive's timestamp range and its
    /// range-index leaves against the range index, before the schema layer is consulted.
    ///
    /// # Returns
    ///
    /// The prune report if the archive cannot match, or `None` if planning must continue.
    #[must_use]
    pub fn prune_on_metadata(&self, metadata: &ArchiveMetadata) -> Option<PruneReport> {
        prune_on_metadata(&self.query, metadata)
    }

    /// # Returns
    ///
    /// The planner of stages B-D for the archive of `tables`.
    #[must_use]
    pub fn plan_archive(&self, tables: Arc<ArchiveTables>) -> ArchivePlanner {
        ArchivePlanner {
            query: Arc::clone(&self.query),
            projection: self.projection.clone(),
            limits: self.limits,
            state: State::Start(tables),
        }
    }
}

/// The sans-IO planner of one archive (stages B-D); drive it with [`ArchivePlanner::advance`].
#[derive(Debug)]
pub struct ArchivePlanner {
    query: Arc<Query>,
    projection: RecordProjection,
    limits: PlanLimits,
    state: State,
}

impl ArchivePlanner {
    /// Advances planning. After [`PlanStep::NeedsDictionaries`], the caller loads the requested
    /// dictionaries into the archive's [`clp_s_format::ArchiveDictionaries`] and calls `advance`
    /// again.
    ///
    /// # Returns
    ///
    /// The next step on success; [`PlanStep::Pruned`] and [`PlanStep::Planned`] are final, and
    /// advancing again returns them again.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`SchemaStage::run`]'s return values on failure.
    /// * Forwards [`SchemaStage::finish`]'s return values on failure.
    pub fn advance(&mut self) -> Result<PlanStep, PlanError> {
        let outcome = match &mut self.state {
            State::Start(tables) => {
                if let Some(report) = prune_on_metadata(&self.query, tables.index().metadata()) {
                    SchemaStageOutcome::Pruned(report)
                } else {
                    SchemaStage::run(
                        Arc::clone(&self.query),
                        &self.projection,
                        self.limits,
                        Arc::clone(tables),
                    )?
                }
            }
            State::AwaitingDictionaries(stage) => stage.finish()?,
            State::Pruned(report) => return Ok(PlanStep::Pruned(*report)),
            State::Planned(plan) => return Ok(PlanStep::Planned(Arc::clone(plan))),
        };
        let (state, step) = match outcome {
            SchemaStageOutcome::Pruned(report) => (State::Pruned(report), PlanStep::Pruned(report)),
            SchemaStageOutcome::Planned(plan) => {
                (State::Planned(Arc::clone(&plan)), PlanStep::Planned(plan))
            }
            SchemaStageOutcome::NeedsDictionaries(stage) => {
                let needs = stage.dictionary_needs();
                (
                    State::AwaitingDictionaries(stage),
                    PlanStep::NeedsDictionaries(needs),
                )
            }
        };
        self.state = state;
        Ok(step)
    }
}

/// One step of planning an archive.
#[derive(Debug)]
pub enum PlanStep {
    /// Load these dictionaries, then advance again.
    NeedsDictionaries(DictionaryNeeds),

    /// The archive cannot match.
    Pruned(PruneReport),

    /// The archive's plan.
    Planned(Arc<ArchiveSearchPlan>),
}

/// The state of an [`ArchivePlanner`].
enum State {
    Start(Arc<ArchiveTables>),
    AwaitingDictionaries(Box<SchemaStage>),
    Pruned(PruneReport),
    Planned(Arc<ArchiveSearchPlan>),
}

impl std::fmt::Debug for State {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Start(_) => "Start",
            Self::AwaitingDictionaries(_) => "AwaitingDictionaries",
            Self::Pruned(_) => "Pruned",
            Self::Planned(_) => "Planned",
        })
    }
}

/// Stage A: archive-scope outcomes for time leaves (against the authoritative timestamp range)
/// and range-index leaves (against every range), every other leaf being unconstrained.
///
/// # Returns
///
/// The prune report if the root cannot be True, else `None`.
fn prune_on_metadata(query: &Query, metadata: &ArchiveMetadata) -> Option<PruneReport> {
    if NormalExpr::MatchNone == *query.root() {
        return Some(PruneReport {
            stage: TerminationStage::LogicallyFalse,
            reason: PruneReason::LogicallyFalse,
        });
    }
    let authoritative = metadata.timestamp_index().authoritative();
    let time_outcomes: Vec<OutcomeSet> = query
        .leaves()
        .iter()
        .map(|leaf| match leaf.column {
            ColumnRef::AuthoritativeTimestamp => {
                authoritative_timestamp_outcomes(leaf, authoritative)
            }
            _ => OutcomeSet::ALL,
        })
        .collect();
    if !may_be_true(query.root(), &time_outcomes) {
        let reason = if authoritative.is_none() {
            PruneReason::NoTimestampColumn
        } else {
            PruneReason::TimeRangeDisjoint
        };
        return Some(PruneReport {
            stage: TerminationStage::TimeRangeMatching,
            reason,
        });
    }

    let mut has_range_leaf = false;
    let outcomes: Vec<OutcomeSet> = query
        .leaves()
        .iter()
        .zip(time_outcomes)
        .map(|(leaf, outcomes)| match leaf.column {
            ColumnRef::RangeIndexField(_) => {
                has_range_leaf = true;
                range_index_outcomes(leaf, metadata.range_index()).1
            }
            _ => outcomes,
        })
        .collect();
    if has_range_leaf && !may_be_true(query.root(), &outcomes) {
        return Some(PruneReport {
            stage: TerminationStage::RangeIndexMatching,
            reason: PruneReason::RangeIndexMismatch,
        });
    }
    None
}

/// # Returns
///
/// Whether the root can be True when each leaf `i` takes values in `outcomes[i]`.
fn may_be_true(root: &NormalExpr, outcomes: &[OutcomeSet]) -> bool {
    tree::fold(root, |node| match node {
        FoldNode::Constant(value) => value,
        FoldNode::Leaf { leaf, is_negated } => outcomes
            .get(leaf.get() as usize)
            .is_some_and(|set| set.contains(OutcomeSet::selected_value(is_negated))),
        FoldNode::And(children) => children.into_iter().all(|child| child),
        FoldNode::Or(children) => children.into_iter().any(|child| child),
    })
}
