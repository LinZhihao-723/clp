//! Stages B-D of planning one archive: per-table outcome sets from the schema layer (with
//! timestamp-index refinement), the dictionary prefilter, and the per-table fold into shared
//! residual templates with column bindings.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use clp_s_format::ArchiveIndex;
use clp_s_format::DictionaryKind;
use clp_s_format::DictionaryNeeds;
use clp_s_format::LogTypeDictionary;
use clp_s_format::NodeId;
use clp_s_format::NodeType;
use clp_s_format::TableOrdinal;
use clp_s_format::VariableDictionary;
use clp_s_query::ColumnRef;
use clp_s_query::CompareOperator;
use clp_s_query::Leaf;
use clp_s_query::LeafId;
use clp_s_query::NormalExpr;
use clp_s_query::PatternPiece;
use clp_s_query::PredicateTest;
use clp_s_query::Query;
use clp_s_query::QueryValue;
use clp_s_query::ScalarValue;
use clp_s_query::SearchType;
use clp_s_query::TextPattern;
use clp_s_query::TruthValue;
use clp_s_table::ArchiveTables;
use clp_s_table::ResolvedProjection;
use clp_s_table::TableLayout;

use crate::ArchiveSearchPlan;
use crate::BoundColumn;
use crate::ColumnTest;
use crate::EncodedTextMatch;
use crate::EncodedTextMember;
use crate::IdSet;
use crate::LeafBinding;
use crate::PlanError;
use crate::PlanLimits;
use crate::PruneReason;
use crate::PruneReport;
use crate::RangeOutcomes;
use crate::RecordProjection;
use crate::ResidualFilter;
use crate::ResidualTemplate;
use crate::TableFilterPlan;
use crate::TableSearchPlan;
use crate::TerminationStage;
use crate::bits::TableBits;
use crate::metadata::TimestampBounds;
use crate::metadata::range_index_outcomes;
use crate::outcome::OutcomeSet;
use crate::outcome::SingleValue;
use crate::resolve::ColumnResolver;
use crate::resolve::NodeAccess;
use crate::tree;
use crate::tree::FoldNode;
use crate::tree::Folded;
use crate::tree::OccurrenceState;

/// The result of the schema stage: either final, or waiting for dictionaries.
pub enum SchemaStageOutcome {
    Pruned(PruneReport),
    Planned(Arc<ArchiveSearchPlan>),
    NeedsDictionaries(Box<SchemaStage>),
}

/// The planning state of one archive after Stage B.
pub struct SchemaStage {
    query: Arc<Query>,
    tables: Arc<ArchiveTables>,
    limits: PlanLimits,
    projection: ResolvedProjection,
    leaves: Vec<LeafPlan>,
    presence: PresenceIndex,
    candidates: TableBits,
}

impl SchemaStage {
    /// Runs Stage B: resolves every distinct column once, derives per-node and per-table outcome
    /// sets (with timestamp-index refinement) and the candidate tables.
    ///
    /// # Returns
    ///
    /// The prune report if no table can match, the plan if no dictionary is needed, or the
    /// stage waiting for the dictionaries it needs, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::finish`]'s return values on failure.
    pub fn run(
        query: Arc<Query>,
        projection: &RecordProjection,
        limits: PlanLimits,
        tables: Arc<ArchiveTables>,
    ) -> Result<SchemaStageOutcome, PlanError> {
        let index = Arc::clone(tables.index());
        let num_tables = u32::try_from(index.tables().tables().len())
            .expect("the format layer bounds the table count by u32::MAX");
        let resolver = ColumnResolver::new(index.schema_tree(), index.metadata().timestamp_index());
        let resolved_projection = resolve_projection(&resolver, projection);

        let mut presence = PresenceIndex::new(num_tables);
        let mut nodes_by_column: HashMap<&ColumnRef, Vec<NodeArc>> = HashMap::new();
        let mut leaves = Vec::with_capacity(query.leaves().len());
        for leaf in query.leaves() {
            let nodes = nodes_by_column
                .entry(&leaf.column)
                .or_insert_with(|| {
                    resolver
                        .resolve_filter(&leaf.column)
                        .into_iter()
                        .filter_map(|binding| {
                            let node_type = index.schema_tree().node(binding.node)?.node_type;
                            Some(Arc::new(ResolvedNode {
                                node: binding.node,
                                node_type,
                                access: binding.access,
                            }))
                        })
                        .collect()
                })
                .clone();
            leaves.push(LeafPlan::new(leaf, nodes, &index, &mut presence));
        }

        let mut stage = Self {
            query,
            tables,
            limits,
            projection: resolved_projection,
            leaves,
            presence,
            candidates: TableBits::empty(num_tables),
        };
        let (candidates, _) = stage.root_tables(true);
        if candidates.is_empty() {
            let (unrefined, _) = stage.root_tables(false);
            let report = if unrefined.is_empty() {
                PruneReport {
                    stage: TerminationStage::SchemaMatching,
                    reason: PruneReason::NoCandidateTable,
                }
            } else {
                PruneReport {
                    stage: TerminationStage::TimeRangeMatchingAfterColumnResolution,
                    reason: PruneReason::TimeRangeDisjoint,
                }
            };
            return Ok(SchemaStageOutcome::Pruned(report));
        }
        stage.candidates = candidates;

        let needs = stage.dictionary_needs();
        let dictionaries = stage.tables.dictionaries();
        let is_missing = (needs.is_variable_needed && dictionaries.variable.get().is_none())
            || (needs.is_logtype_needed && dictionaries.logtype.get().is_none());
        if is_missing {
            return Ok(SchemaStageOutcome::NeedsDictionaries(Box::new(stage)));
        }
        stage.finish()
    }

    /// # Returns
    ///
    /// The dictionaries Stage C reads.
    pub fn dictionary_needs(&self) -> DictionaryNeeds {
        let mut needs = DictionaryNeeds::default();
        for leaf in &self.leaves {
            for node in &leaf.nodes {
                if !node.is_pending_dictionary()
                    || !self.presence.intersects(node, &self.candidates)
                {
                    continue;
                }
                needs.is_variable_needed = true;
                needs.is_logtype_needed |= NodeType::ClpString == node.resolved.node_type;
            }
        }
        needs
    }

    /// Runs Stage C (dictionary prefilter) and Stage D (fold into templates and bindings).
    ///
    /// # Returns
    ///
    /// The prune report if the dictionaries rule every candidate out, else the plan, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`PlanError::DictionaryNotLoaded`] if a dictionary Stage C reads is not loaded.
    /// * Forwards [`Self::run_dictionary_stage`]'s return values on failure.
    /// * Forwards [`Self::fold_tables`]'s return values on failure.
    pub fn finish(&mut self) -> Result<SchemaStageOutcome, PlanError> {
        let needs = self.dictionary_needs();
        if needs.is_variable_needed || needs.is_logtype_needed {
            let dictionaries = Arc::clone(self.tables.dictionaries());
            let variables = if needs.is_variable_needed {
                Some(Arc::clone(dictionaries.variable.get().ok_or(
                    PlanError::DictionaryNotLoaded(DictionaryKind::Variable),
                )?))
            } else {
                None
            };
            let logtypes = if needs.is_logtype_needed {
                Some(Arc::clone(dictionaries.logtype.get().ok_or(
                    PlanError::DictionaryNotLoaded(DictionaryKind::LogType),
                )?))
            } else {
                None
            };
            self.run_dictionary_stage(variables.as_deref(), logtypes.as_deref())?;
            let (candidates, _) = self.root_tables(true);
            if candidates.is_empty() {
                return Ok(SchemaStageOutcome::Pruned(PruneReport {
                    stage: TerminationStage::DictionarySearch,
                    reason: PruneReason::DictionaryMiss,
                }));
            }
            self.candidates = candidates;
        }
        self.fold_tables()
    }

    /// Computes the `(may, must)` tables of the query root: may-be-True and always-True.
    ///
    /// # Returns
    ///
    /// The candidate tables and the tables every row of which matches.
    fn root_tables(&self, is_timestamp_refined: bool) -> (TableBits, TableBits) {
        let num_tables = self.presence.num_tables;
        let outcomes: Vec<TableOutcomes> = self
            .leaves
            .iter()
            .map(|leaf| leaf.table_outcomes(&self.presence, is_timestamp_refined))
            .collect();
        tree::fold(self.query.root(), |node| match node {
            FoldNode::Constant(true) => (TableBits::full(num_tables), TableBits::full(num_tables)),
            FoldNode::Constant(false) => {
                (TableBits::empty(num_tables), TableBits::empty(num_tables))
            }
            FoldNode::Leaf { leaf, is_negated } => outcomes.get(leaf.get() as usize).map_or_else(
                || (TableBits::empty(num_tables), TableBits::empty(num_tables)),
                |outcomes| outcomes.occurrence(is_negated),
            ),
            FoldNode::And(children) => combine(children, num_tables, true),
            FoldNode::Or(children) => combine(children, num_tables, false),
        })
    }

    /// Stage C: `VarString` id sets and `ClpString` prefilters for every leaf with a pending text
    /// node on a candidate table, then refines those nodes.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`PlanError::DictionaryNotLoaded`] if a needed dictionary is `None`.
    /// * Forwards [`check_limit`]'s return values on failure.
    fn run_dictionary_stage(
        &mut self,
        variables: Option<&VariableDictionary>,
        logtypes: Option<&LogTypeDictionary>,
    ) -> Result<(), PlanError> {
        let mut plan_size = 0_u64;
        for leaf_index in 0..self.leaves.len() {
            let pending: Vec<NodeType> = self.leaves[leaf_index]
                .nodes
                .iter()
                .filter(|node| {
                    node.is_pending_dictionary() && self.presence.intersects(node, &self.candidates)
                })
                .map(|node| node.resolved.node_type)
                .collect();
            if pending.is_empty() {
                continue;
            }
            let leaf = &mut self.leaves[leaf_index];
            if pending.contains(&NodeType::VarString) {
                let variables =
                    variables.ok_or(PlanError::DictionaryNotLoaded(DictionaryKind::Variable))?;
                let ids = leaf.text.variable_ids(variables);
                plan_size += ids.as_ref().map_or(0, IdSet::heap_size);
                leaf.variable_ids = ids.map_or(Lookup::Missed, |ids| Lookup::Found(Arc::new(ids)));
            }
            if pending.contains(&NodeType::ClpString) {
                let variables =
                    variables.ok_or(PlanError::DictionaryNotLoaded(DictionaryKind::Variable))?;
                let logtypes =
                    logtypes.ok_or(PlanError::DictionaryNotLoaded(DictionaryKind::LogType))?;
                let members = leaf.text.encoded_text_members(logtypes, variables);
                plan_size += members
                    .iter()
                    .map(|member| member.prefilter.heap_size())
                    .sum::<u64>();
                leaf.encoded_text = if members.is_empty() {
                    Lookup::Missed
                } else {
                    Lookup::Found(Arc::from(members))
                };
            }
            leaf.refine_text_nodes();
            check_limit(plan_size, self.limits)?;
        }
        Ok(())
    }

    /// Stage D: classifies every candidate table as match-all or residual, folds residual tables
    /// into shared templates (memoized by occurrence states), and binds their leaves to columns.
    ///
    /// # Returns
    ///
    /// The plan (or a prune report if no table is left) on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`PlanError::Internal`] if a candidate table is not in the table directory.
    /// * Forwards [`check_limit`]'s return values on failure.
    /// * Forwards [`ArchiveTables::layout`]'s return values on failure.
    /// * Forwards [`TemplateCache::template`]'s return values on failure.
    /// * Forwards [`LeafPlan::bind`]'s return values on failure.
    fn fold_tables(&self) -> Result<SchemaStageOutcome, PlanError> {
        let (candidates, match_all) = self.root_tables(true);
        let outcomes: Vec<TableOutcomes> = self
            .leaves
            .iter()
            .map(|leaf| leaf.table_outcomes(&self.presence, true))
            .collect();
        let mut templates = TemplateCache::new(self.query.root());
        let mut plans = Vec::with_capacity(candidates.len() as usize);
        let mut filter_needs = DictionaryNeeds::default();
        let mut plan_size = self.leaves.iter().map(LeafPlan::heap_size).sum::<u64>();
        let directory = self.tables.index().tables();

        for table in candidates.iter() {
            let info = directory.table(table).ok_or(PlanError::Internal {
                reason: "a candidate table is not in the table directory",
            })?;
            if match_all.contains(table) {
                plans.push(TableSearchPlan {
                    table,
                    num_rows: info.num_rows,
                    filter: TableFilterPlan::MatchAll,
                });
                continue;
            }
            let template = templates.template(&outcomes, table)?;
            let layout = self.tables.layout(table)?;
            let mut bindings: Vec<LeafBinding> = Vec::new();
            let mut bound_leaves = HashSet::new();
            for (leaf, _) in tree::occurrences(template.root()) {
                if !bound_leaves.insert(leaf) {
                    continue;
                }
                let leaf_plan =
                    self.leaves
                        .get(leaf.get() as usize)
                        .ok_or(PlanError::Internal {
                            reason: "a template leaf is not a leaf of the query",
                        })?;
                let binding = leaf_plan.bind(leaf, table, &layout, &self.presence)?;
                for column in &binding.columns {
                    filter_needs = filter_needs.union(column_dictionary_needs(column, &layout));
                }
                plan_size += (std::mem::size_of::<LeafBinding>()
                    + binding.columns.len() * std::mem::size_of::<BoundColumn>())
                    as u64;
                bindings.push(binding);
            }
            check_limit(plan_size, self.limits)?;
            plans.push(TableSearchPlan {
                table,
                num_rows: info.num_rows,
                filter: TableFilterPlan::Residual(ResidualFilter::new(template, bindings)),
            });
        }

        if plans.is_empty() {
            return Ok(SchemaStageOutcome::Pruned(PruneReport {
                stage: TerminationStage::SchemaMatching,
                reason: PruneReason::NoCandidateTable,
            }));
        }
        Ok(SchemaStageOutcome::Planned(Arc::new(
            ArchiveSearchPlan::new(
                Arc::clone(&self.tables),
                Arc::clone(&self.query),
                plans.into_boxed_slice(),
                templates.into_templates(),
                self.projection.clone(),
                filter_needs,
            ),
        )))
    }
}

/// Residual templates, memoized by the occurrence states of a table and deduplicated by root.
struct TemplateCache<'query> {
    root: &'query NormalExpr,
    occurrences: Vec<(LeafId, bool)>,
    positions: HashMap<(LeafId, bool), usize>,
    folds: HashMap<Vec<OccurrenceState>, Arc<ResidualTemplate>>,
    templates: Vec<Arc<ResidualTemplate>>,
    templates_by_root: HashMap<NormalExpr, Arc<ResidualTemplate>>,
}

impl<'query> TemplateCache<'query> {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// An empty cache for the query tree `root`.
    fn new(root: &'query NormalExpr) -> Self {
        let occurrences = tree::occurrences(root);
        let positions = occurrences
            .iter()
            .enumerate()
            .map(|(position, occurrence)| (*occurrence, position))
            .collect();
        Self {
            root,
            occurrences,
            positions,
            folds: HashMap::new(),
            templates: Vec::new(),
            templates_by_root: HashMap::new(),
        }
    }

    /// # Returns
    ///
    /// The template of `table`, a candidate that is not match-all, whose leaves have `outcomes`,
    /// on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`PlanError::Internal`] if the tree folds to a constant on `table`, which the table
    ///   bitsets rule out for a candidate that is not match-all.
    fn template(
        &mut self,
        outcomes: &[TableOutcomes],
        table: TableOrdinal,
    ) -> Result<Arc<ResidualTemplate>, PlanError> {
        let states: Vec<OccurrenceState> = self
            .occurrences
            .iter()
            .map(|(leaf, is_negated)| {
                outcomes
                    .get(leaf.get() as usize)
                    .map_or(OccurrenceState::Never, |outcomes| {
                        outcomes.state(table, *is_negated)
                    })
            })
            .collect();
        if let Some(template) = self.folds.get(&states) {
            return Ok(Arc::clone(template));
        }
        let folded = tree::fold_residual(self.root, |leaf, is_negated| {
            let position = self
                .positions
                .get(&(leaf, is_negated))
                .expect("every occurrence of the root is indexed");
            states[*position]
        });
        let Folded::Residual(root) = folded else {
            return Err(PlanError::Internal {
                reason: "a candidate table that is not match-all folds to a constant",
            });
        };
        let next_index = self.templates.len();
        let template = Arc::clone(
            self.templates_by_root
                .entry(root.clone())
                .or_insert_with(|| Arc::new(ResidualTemplate::new(next_index, root))),
        );
        if next_index == template.index() {
            self.templates.push(Arc::clone(&template));
        }
        self.folds.insert(states, Arc::clone(&template));
        Ok(template)
    }

    /// # Returns
    ///
    /// The distinct templates, in creation order.
    fn into_templates(self) -> Box<[Arc<ResidualTemplate>]> {
        self.templates.into_boxed_slice()
    }
}

/// A resolved node, shared by every leaf on the same column reference.
#[derive(Debug)]
struct ResolvedNode {
    node: NodeId,
    node_type: NodeType,
    access: NodeAccess,
}

type NodeArc = Arc<ResolvedNode>;

/// What Stage C still has to decide for a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextState {
    /// The node's outcomes do not depend on the dictionaries.
    Settled,

    /// A `VarString` or `ClpString` node whose rows are True or False depending on the
    /// dictionaries.
    Pending,

    /// Stage C ran; the prefilter is bound to the node.
    Prefiltered,
}

/// One node a leaf reaches, with its outcomes.
#[derive(Debug)]
struct NodePlan {
    resolved: NodeArc,

    /// The outcomes with timestamp-index refinement.
    outcomes: OutcomeSet,

    /// The outcomes without timestamp-index refinement.
    unrefined_outcomes: OutcomeSet,

    text_state: TextState,
}

impl NodePlan {
    /// # Returns
    ///
    /// Whether Stage C must decide this node.
    fn is_pending_dictionary(&self) -> bool {
        TextState::Pending == self.text_state
    }
}

/// The archive-bound state of one leaf.
struct LeafPlan {
    leaf: Leaf,
    nodes: Vec<NodePlan>,
    text: TextMembers,

    /// For range-index leaves: the per-range outcomes and the archive-scope set.
    range: Option<(Arc<RangeOutcomes>, OutcomeSet)>,

    variable_ids: Lookup<Arc<IdSet>>,
    encoded_text: Lookup<Arc<[EncodedTextMember]>>,
}

impl LeafPlan {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The plan of `leaf`, which reaches `nodes`, with the Stage B outcomes of every node.
    fn new(
        leaf: &Leaf,
        nodes: Vec<NodeArc>,
        index: &ArchiveIndex,
        presence: &mut PresenceIndex,
    ) -> Self {
        let text = TextMembers::of_leaf(leaf);
        let timestamps = index.metadata().timestamp_index();
        let mut node_plans = Vec::with_capacity(nodes.len());
        for resolved in nodes {
            presence.ensure(index, resolved.node, resolved.node_type);
            let (unrefined_outcomes, text_state) = node_outcomes(leaf, &resolved, &text);
            let mut outcomes = unrefined_outcomes;
            let bounds = timestamps
                .entries()
                .iter()
                .find(|entry| entry.node_id == resolved.node)
                .and_then(TimestampBounds::of_entry);
            if let (Some(bounds), NodeType::Timestamp, NodeAccess::Value) =
                (bounds, resolved.node_type, &resolved.access)
                && OutcomeSet::TRUE_OR_FALSE == outcomes
            {
                outcomes = bounds.outcomes(leaf);
            }
            node_plans.push(NodePlan {
                resolved,
                outcomes,
                unrefined_outcomes,
                text_state,
            });
        }

        let range = if let ColumnRef::RangeIndexField(_) = leaf.column {
            if let Some(node) = index.schema_tree().log_event_idx_node() {
                presence.ensure(index, node, NodeType::Integer);
            }
            let (outcomes, set) = range_index_outcomes(leaf, index.metadata().range_index());
            Some((Arc::new(outcomes), set))
        } else {
            None
        };

        Self {
            leaf: leaf.clone(),
            nodes: node_plans,
            text,
            range,
            variable_ids: Lookup::NotRun,
            encoded_text: Lookup::NotRun,
        }
    }

    /// Refines the pending text nodes with the Stage C results: a node whose lookup found nothing
    /// is False on every row.
    fn refine_text_nodes(&mut self) {
        for node in &mut self.nodes {
            if TextState::Pending != node.text_state {
                continue;
            }
            let result = match node.resolved.node_type {
                NodeType::VarString => self.variable_ids.is_found(),
                NodeType::ClpString => self.encoded_text.is_found(),
                _ => None,
            };
            match result {
                Some(true) => node.text_state = TextState::Prefiltered,
                Some(false) => {
                    node.text_state = TextState::Prefiltered;
                    node.outcomes = OutcomeSet::FALSE;
                    node.unrefined_outcomes = OutcomeSet::FALSE;
                }
                None => {}
            }
        }
    }

    /// # Returns
    ///
    /// The outcomes of the leaf on every table.
    fn table_outcomes(
        &self,
        presence: &PresenceIndex,
        is_timestamp_refined: bool,
    ) -> TableOutcomes {
        let num_tables = presence.num_tables;
        if let Some((ranges, set)) = &self.range {
            let present = presence.log_event_idx_tables();
            let outside = if ranges.is_false_outside_ranges {
                TruthValue::False
            } else {
                TruthValue::Unknown
            };
            let mut outcomes = TableOutcomes {
                truthy: TableBits::empty(num_tables),
                falsy: TableBits::empty(num_tables),
                unknown: TableBits::empty(num_tables),
            };
            for (value, tables) in [
                (TruthValue::True, &mut outcomes.truthy),
                (TruthValue::False, &mut outcomes.falsy),
                (TruthValue::Unknown, &mut outcomes.unknown),
            ] {
                if outside == value {
                    *tables = TableBits::full(num_tables);
                } else if set.contains(value) {
                    tables.union_with(&present);
                }
            }
            return outcomes;
        }

        let mut present_any = TableBits::empty(num_tables);
        let mut true_any = TableBits::empty(num_tables);
        let mut false_any = TableBits::empty(num_tables);
        let mut always_true_any = TableBits::empty(num_tables);
        let mut never_unknown_any = TableBits::empty(num_tables);
        for node in &self.nodes {
            let tables = presence.tables(node.resolved.node);
            let outcomes = if is_timestamp_refined {
                node.outcomes
            } else {
                node.unrefined_outcomes
            };
            present_any.union_with(tables);
            if outcomes.contains(TruthValue::True) {
                true_any.union_with(tables);
            }
            if outcomes.contains(TruthValue::False) {
                false_any.union_with(tables);
            }
            if OutcomeSet::TRUE == outcomes {
                always_true_any.union_with(tables);
            }
            if !outcomes.contains(TruthValue::Unknown) {
                never_unknown_any.union_with(tables);
            }
        }
        let absent = present_any.complement();
        let mut falsy = false_any;
        falsy.subtract(&always_true_any);
        let mut unknown = present_any;
        unknown.subtract(&never_unknown_any);
        if PredicateTest::Exists == self.leaf.test {
            falsy.union_with(&absent);
        } else {
            unknown.union_with(&absent);
        }
        TableOutcomes {
            truthy: true_any,
            falsy,
            unknown,
        }
    }

    /// Binds the leaf, which varies on `table`, to the columns of `table`.
    ///
    /// # Returns
    ///
    /// The binding on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`PlanError::Internal`] if the leaf is constant on `table`, or a node that varies has no
    ///   column or no dictionary lookup.
    fn bind(
        &self,
        leaf: LeafId,
        table: TableOrdinal,
        layout: &TableLayout,
        presence: &PresenceIndex,
    ) -> Result<LeafBinding, PlanError> {
        let internal = |reason| Err(PlanError::Internal { reason });
        if let Some((ranges, _)) = &self.range {
            // Without a `log_event_idx` column, every row lies outside every range, so the leaf
            // is constant on the table.
            let (Some(column), Some(node_id)) =
                (layout.log_event_idx_column(), presence.log_event_idx_node)
            else {
                return internal("a range-index leaf varies on a table without log_event_idx");
            };
            return Ok(LeafBinding {
                leaf,
                columns: vec![BoundColumn {
                    column,
                    node_id,
                    test: ColumnTest::LogEventRanges(Arc::clone(ranges)),
                }]
                .into_boxed_slice(),
                is_false_without_columns: false,
            });
        }
        let mut columns = Vec::new();
        let mut is_false_without_columns = false;
        for node in &self.nodes {
            if !presence.tables(node.resolved.node).contains(table) {
                continue;
            }
            if OutcomeSet::FALSE == node.outcomes {
                is_false_without_columns = true;
                continue;
            }
            if OutcomeSet::UNKNOWN == node.outcomes {
                continue;
            }
            if OutcomeSet::TRUE == node.outcomes {
                return internal("a node that is True on every row makes its leaf constant");
            }
            let Some(column) = layout.columns_for_node(node.resolved.node).first().copied() else {
                return internal("a node whose outcomes vary has no column");
            };
            let test = match (&node.resolved.access, node.resolved.node_type) {
                (NodeAccess::Array(access), _) => ColumnTest::Array(access.clone()),
                (NodeAccess::Value, NodeType::VarString) => match &self.variable_ids {
                    Lookup::Found(ids) => ColumnTest::VariableIds(Arc::clone(ids)),
                    Lookup::NotRun | Lookup::Missed => {
                        return internal("a VarString node that varies has no id set");
                    }
                },
                (NodeAccess::Value, NodeType::ClpString) => match &self.encoded_text {
                    Lookup::Found(members) => ColumnTest::EncodedText(Arc::clone(members)),
                    Lookup::NotRun | Lookup::Missed => {
                        return internal("a ClpString node that varies has no prefilter");
                    }
                },
                (NodeAccess::Value, _) => ColumnTest::Value,
            };
            columns.push(BoundColumn {
                column,
                node_id: node.resolved.node,
                test,
            });
        }
        columns.sort_by_key(|column| column.column);
        Ok(LeafBinding {
            leaf,
            columns: columns.into_boxed_slice(),
            is_false_without_columns,
        })
    }

    /// # Returns
    ///
    /// The heap bytes of the leaf's shared plan data.
    fn heap_size(&self) -> u64 {
        let ids = match &self.variable_ids {
            Lookup::Found(ids) => ids.heap_size(),
            Lookup::NotRun | Lookup::Missed => 0,
        };
        let text = match &self.encoded_text {
            Lookup::Found(members) => members
                .iter()
                .map(|member| member.prefilter.heap_size())
                .sum(),
            Lookup::NotRun | Lookup::Missed => 0,
        };
        let ranges = self.range.as_ref().map_or(0, |(ranges, _)| {
            ((ranges.true_ranges.len() + ranges.false_ranges.len()) * 16) as u64
        });
        ids + text + ranges
    }
}

/// The result of a Stage C lookup.
enum Lookup<ValueType> {
    NotRun,
    Missed,
    Found(ValueType),
}

impl<ValueType> Lookup<ValueType> {
    /// # Returns
    ///
    /// Whether the lookup found something, or `None` if it has not run.
    const fn is_found(&self) -> Option<bool> {
        match self {
            Self::NotRun => None,
            Self::Missed => Some(false),
            Self::Found(_) => Some(true),
        }
    }
}

/// The text forms of a leaf's literals, as the dictionary prefilter sees them.
#[derive(Debug, Default)]
struct TextMembers {
    is_case_sensitive: bool,

    /// Whether the literal is comparable with text at all.
    is_comparable: bool,

    /// Whether some member matches every text.
    is_match_all: bool,

    /// The members a `VarString` (a text without a space) can match.
    variable_patterns: Vec<TextPattern>,

    /// The members a `ClpString` (a text with a space) can match.
    encoded_patterns: Vec<TextPattern>,
}

impl TextMembers {
    /// # Returns
    ///
    /// The text members of `leaf`'s literals that are comparable with text values.
    fn of_leaf(leaf: &Leaf) -> Self {
        let (operator, values, is_case_sensitive): (CompareOperator, Vec<&QueryValue>, bool) =
            match &leaf.test {
                PredicateTest::Compare {
                    operator,
                    value,
                    is_case_sensitive,
                } => (*operator, vec![value], *is_case_sensitive),
                PredicateTest::InSet {
                    values,
                    is_case_sensitive,
                } => (
                    CompareOperator::Equal,
                    values.iter().collect(),
                    *is_case_sensitive,
                ),
                _ => return Self::default(),
            };
        let mut members = Self {
            is_case_sensitive,
            ..Self::default()
        };
        for value in values {
            if !value
                .comparable_search_types(operator)
                .contains(SearchType::Text)
            {
                continue;
            }
            let pattern = match value {
                QueryValue::Integer { text, .. } | QueryValue::Float { text, .. } => {
                    literal_pattern(text.as_bytes())
                }
                QueryValue::Boolean(true) => literal_pattern(b"true"),
                QueryValue::Boolean(false) => literal_pattern(b"false"),
                QueryValue::Null => literal_pattern(b"null"),
                QueryValue::Text(pattern) => pattern.clone(),
                _ => continue,
            };
            members.is_comparable = true;
            if [PatternPiece::AnyRun] == pattern.pieces() {
                members.is_match_all = true;
            }
            let has_literal_space = pattern.pieces().iter().any(
                |piece| matches!(piece, PatternPiece::Literal(bytes) if bytes.contains(&b' ')),
            );
            if !has_literal_space {
                members.variable_patterns.push(pattern.clone());
            }
            if has_literal_space || pattern.has_wildcards() {
                members.encoded_patterns.push(pattern);
            }
        }
        members
    }

    /// # Returns
    ///
    /// The outcomes of the members on a text node of `node_type` before the dictionaries are
    /// read, and whether the dictionaries can refine them.
    fn node_outcomes(&self, node_type: NodeType) -> (OutcomeSet, TextState) {
        if !self.is_comparable {
            return (OutcomeSet::UNKNOWN, TextState::Settled);
        }
        if self.is_match_all {
            return (OutcomeSet::TRUE, TextState::Settled);
        }
        let patterns = if NodeType::VarString == node_type {
            &self.variable_patterns
        } else {
            &self.encoded_patterns
        };
        if patterns.is_empty() {
            (OutcomeSet::FALSE, TextState::Settled)
        } else {
            (OutcomeSet::TRUE_OR_FALSE, TextState::Pending)
        }
    }

    /// # Returns
    ///
    /// The ids of the variable-dictionary entries some `VarString` member matches, or `None` if
    /// none.
    fn variable_ids(&self, variables: &VariableDictionary) -> Option<IdSet> {
        let mut ids: Vec<u64> = Vec::new();
        let mut exact: Vec<&[u8]> = Vec::new();
        let mut globs: Vec<&TextPattern> = Vec::new();
        for pattern in &self.variable_patterns {
            match pattern.as_literal() {
                Some(literal) => exact.push(literal),
                None => globs.push(pattern),
            }
        }
        if !exact.is_empty() {
            for matched in variables
                .find_exact(&exact, self.is_case_sensitive)
                .values()
            {
                ids.extend(matched.iter().map(|id| id.get()));
            }
        }
        if !globs.is_empty() {
            variables.for_each_entry(|id, entry| {
                if globs
                    .iter()
                    .any(|glob| glob.matches(entry, self.is_case_sensitive))
                {
                    ids.push(id.get());
                }
                true
            });
        }
        IdSet::from_ids(ids)
    }

    /// # Returns
    ///
    /// The `ClpString` members with their prefilters, without the members that match nothing.
    fn encoded_text_members(
        &self,
        logtypes: &LogTypeDictionary,
        variables: &VariableDictionary,
    ) -> Vec<EncodedTextMember> {
        self.encoded_patterns
            .iter()
            .filter_map(|pattern| {
                let prefilter =
                    EncodedTextMatch::new(pattern, self.is_case_sensitive, logtypes, variables);
                (EncodedTextMatch::Never != prefilter).then(|| EncodedTextMember {
                    pattern: pattern.clone(),
                    prefilter,
                })
            })
            .collect()
    }
}

/// The outcomes of one leaf per table, as three bitsets.
struct TableOutcomes {
    /// The tables where the leaf can be True.
    truthy: TableBits,

    /// The tables where the leaf can be False.
    falsy: TableBits,

    /// The tables where the leaf can be Unknown.
    unknown: TableBits,
}

impl TableOutcomes {
    /// # Returns
    ///
    /// The `(may, must)` tables of an occurrence: the tables where the occurrence's selected value
    /// is possible, and those where it is the only possible value.
    fn occurrence(&self, is_negated: bool) -> (TableBits, TableBits) {
        let (selected, other) = if is_negated {
            (&self.falsy, &self.truthy)
        } else {
            (&self.truthy, &self.falsy)
        };
        let mut must = selected.clone();
        must.subtract(other);
        must.subtract(&self.unknown);
        (selected.clone(), must)
    }

    /// # Returns
    ///
    /// The state of an occurrence on `table`.
    fn state(&self, table: TableOrdinal, is_negated: bool) -> OccurrenceState {
        let (selected, other) = if is_negated {
            (&self.falsy, &self.truthy)
        } else {
            (&self.truthy, &self.falsy)
        };
        if !selected.contains(table) {
            OccurrenceState::Never
        } else if other.contains(table) || self.unknown.contains(table) {
            OccurrenceState::Varies
        } else {
            OccurrenceState::Always
        }
    }
}

/// The tables in which each node is present: leaves from the node-to-table index, objects from
/// their subtree.
struct PresenceIndex {
    num_tables: u32,
    tables: HashMap<NodeId, TableBits>,
    log_event_idx_node: Option<NodeId>,
    empty: TableBits,
}

impl PresenceIndex {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// An empty index over `num_tables` tables.
    fn new(num_tables: u32) -> Self {
        Self {
            num_tables,
            tables: HashMap::new(),
            log_event_idx_node: None,
            empty: TableBits::empty(num_tables),
        }
    }

    /// Computes the presence of `node` if it is not known yet.
    fn ensure(&mut self, index: &ArchiveIndex, node: NodeId, node_type: NodeType) {
        if Some(node) == index.schema_tree().log_event_idx_node() {
            self.log_event_idx_node = Some(node);
        }
        if self.tables.contains_key(&node) {
            return;
        }
        let mut tables = TableBits::empty(self.num_tables);
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            for table in index.node_tables().tables_for_node(current) {
                tables.insert(*table);
            }
            if node_type.is_container() {
                stack.extend(index.schema_tree().children(current).iter().copied());
            }
        }
        self.tables.insert(node, tables);
    }

    /// # Returns
    ///
    /// The tables `node` is present in.
    fn tables(&self, node: NodeId) -> &TableBits {
        self.tables.get(&node).unwrap_or(&self.empty)
    }

    /// # Returns
    ///
    /// Whether `node` is present in some table of `tables`.
    fn intersects(&self, node: &NodePlan, tables: &TableBits) -> bool {
        let mut present = self.tables(node.resolved.node).clone();
        present.intersect_with(tables);
        !present.is_empty()
    }

    /// # Returns
    ///
    /// The tables holding the `log_event_idx` column.
    fn log_event_idx_tables(&self) -> TableBits {
        self.log_event_idx_node
            .map_or_else(|| self.empty.clone(), |node| self.tables(node).clone())
    }
}

/// # Returns
///
/// The outcomes a leaf can take on `node`'s values before the dictionaries are read, and whether
/// the dictionaries can refine them.
fn node_outcomes(leaf: &Leaf, node: &ResolvedNode, text: &TextMembers) -> (OutcomeSet, TextState) {
    let is_exists = matches!(leaf.test, PredicateTest::Exists);
    if let NodeAccess::Array(access) = &node.access {
        let outcomes = if is_exists {
            if matches!(access, crate::ArrayAccess::Walk(tokens) if tokens.is_empty()) {
                OutcomeSet::TRUE
            } else {
                OutcomeSet::TRUE_OR_FALSE
            }
        } else if leaf.search_types.contains(SearchType::Array) {
            OutcomeSet::ALL
        } else {
            OutcomeSet::UNKNOWN
        };
        return (outcomes, TextState::Settled);
    }
    let settled = |outcomes| (outcomes, TextState::Settled);
    match node.node_type {
        NodeType::Object => settled(OutcomeSet::of(
            SingleValue(ScalarValue::Object).evaluate(leaf),
        )),
        NodeType::NullValue => settled(OutcomeSet::of(
            SingleValue(ScalarValue::Null).evaluate(leaf),
        )),
        _ if is_exists => settled(OutcomeSet::TRUE),
        NodeType::VarString | NodeType::ClpString => text.node_outcomes(node.node_type),
        node_type => settled(scalar_outcomes(leaf, node_type)),
    }
}

/// # Returns
///
/// The outcomes of a comparison leaf on a typed scalar column: Unknown if no member is comparable
/// with the column's search type, False if every comparable member is `null` (which no
/// non-text, non-null value equals), else True or False.
fn scalar_outcomes(leaf: &Leaf, node_type: NodeType) -> OutcomeSet {
    let Some(search_type) = search_type_of(node_type) else {
        return OutcomeSet::UNKNOWN;
    };
    let (operator, values): (CompareOperator, Vec<&QueryValue>) = match &leaf.test {
        PredicateTest::Exists => return OutcomeSet::TRUE,
        PredicateTest::Compare {
            operator, value, ..
        } => (*operator, vec![value]),
        PredicateTest::InSet { values, .. } => (CompareOperator::Equal, values.iter().collect()),
        _ => return OutcomeSet::ALL,
    };
    let mut is_any_comparable = false;
    let mut can_vary = false;
    for value in values {
        if !value
            .comparable_search_types(operator)
            .contains(search_type)
        {
            continue;
        }
        is_any_comparable = true;
        if !matches!(value, QueryValue::Null) {
            can_vary = true;
        }
    }
    if !is_any_comparable {
        OutcomeSet::UNKNOWN
    } else if can_vary {
        OutcomeSet::TRUE_OR_FALSE
    } else {
        OutcomeSet::FALSE
    }
}

/// # Returns
///
/// The search type of a scalar column's values, or `None` for node types without one.
const fn search_type_of(node_type: NodeType) -> Option<SearchType> {
    match node_type {
        NodeType::Integer | NodeType::DeltaInteger => Some(SearchType::Integer),
        NodeType::Float | NodeType::FormattedFloat | NodeType::DictionaryFloat => {
            Some(SearchType::Float)
        }
        NodeType::ClpString | NodeType::VarString => Some(SearchType::Text),
        NodeType::Boolean => Some(SearchType::Boolean),
        NodeType::NullValue => Some(SearchType::Null),
        NodeType::Timestamp => Some(SearchType::Timestamp),
        _ => None,
    }
}

/// # Returns
///
/// The pattern matching exactly `bytes`.
fn literal_pattern(bytes: &[u8]) -> TextPattern {
    TextPattern::new(vec![PatternPiece::Literal(bytes.into())])
}

/// # Returns
///
/// The `(may, must)` of an AND (or OR) of `(may, must)` children.
fn combine(
    children: Vec<(TableBits, TableBits)>,
    num_tables: u32,
    is_and: bool,
) -> (TableBits, TableBits) {
    let mut children = children.into_iter();
    let Some((mut may, mut must)) = children.next() else {
        return if is_and {
            (TableBits::full(num_tables), TableBits::full(num_tables))
        } else {
            (TableBits::empty(num_tables), TableBits::empty(num_tables))
        };
    };
    for (child_may, child_must) in children {
        if is_and {
            may.intersect_with(&child_may);
            must.intersect_with(&child_must);
        } else {
            may.union_with(&child_may);
            must.union_with(&child_must);
        }
    }
    (may, must)
}

/// # Returns
///
/// The dictionaries a kernel reads to evaluate `column` of `layout`.
fn column_dictionary_needs(column: &BoundColumn, layout: &TableLayout) -> DictionaryNeeds {
    match &column.test {
        ColumnTest::EncodedText(_) => NodeType::ClpString.referenced_dictionaries(),
        ColumnTest::Array(_) => NodeType::UnstructuredArray.referenced_dictionaries(),
        ColumnTest::Value => layout
            .columns()
            .get(column.column.get() as usize)
            .map_or_else(DictionaryNeeds::default, |spec| {
                spec.node_type.referenced_dictionaries()
            }),
        ColumnTest::VariableIds(_) | ColumnTest::LogEventRanges(_) => DictionaryNeeds::default(),
    }
}

/// # Returns
///
/// The projection resolved against the archive.
fn resolve_projection(
    resolver: &ColumnResolver<'_>,
    projection: &RecordProjection,
) -> ResolvedProjection {
    match projection {
        RecordProjection::Full => ResolvedProjection::Full,
        RecordProjection::Columns(columns) => {
            let mut nodes: Vec<NodeId> = columns
                .iter()
                .flat_map(|column| resolver.resolve_projection(column))
                .collect();
            nodes.sort_unstable();
            nodes.dedup();
            ResolvedProjection::Nodes(nodes.into_boxed_slice())
        }
    }
}

/// # Errors
///
/// Returns an error if:
///
/// * [`PlanError::LimitExceeded`] if `plan_size` exceeds `limits`.
const fn check_limit(plan_size: u64, limits: PlanLimits) -> Result<(), PlanError> {
    if plan_size > limits.max_plan_size {
        return Err(PlanError::LimitExceeded {
            actual_size: plan_size,
            limit_size: limits.max_plan_size,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use clp_s_format::NodeId;
    use clp_s_format::NodeType;
    use clp_s_format::TableOrdinal;
    use clp_s_query::Query;
    use clp_s_query::QueryOptions;
    use clp_s_query::TruthValue;

    use super::LeafPlan;
    use super::Lookup;
    use super::NodePlan;
    use super::PresenceIndex;
    use super::ResolvedNode;
    use super::TextMembers;
    use super::TextState;
    use crate::bits::TableBits;
    use crate::outcome::OutcomeSet;
    use crate::outcome::existential_join;
    use crate::resolve::NodeAccess;
    use crate::tree::OccurrenceState;

    const NUM_TABLES: u32 = 8;
    const SETS: [OutcomeSet; 7] = [
        OutcomeSet::TRUE,
        OutcomeSet::FALSE,
        OutcomeSet::UNKNOWN,
        OutcomeSet::TRUE_OR_FALSE,
        OutcomeSet::ALL,
        OutcomeSet::TRUE.union(OutcomeSet::UNKNOWN),
        OutcomeSet::FALSE.union(OutcomeSet::UNKNOWN),
    ];

    /// # Returns
    ///
    /// A leaf plan of `kql` over three nodes with the given outcomes, node `i` present in the
    /// tables whose ordinal has bit `i` set.
    fn leaf_plan(kql: &str, outcomes: [OutcomeSet; 3]) -> (LeafPlan, PresenceIndex) {
        let query = Query::from_kql(kql, &QueryOptions::default()).expect("the query should parse");
        let mut presence = PresenceIndex::new(NUM_TABLES);
        let mut nodes = Vec::new();
        for (bit, outcomes) in outcomes.into_iter().enumerate() {
            let node = NodeId::new(u32::try_from(bit).expect("three nodes"));
            let mut tables = TableBits::empty(NUM_TABLES);
            for table in 0..NUM_TABLES {
                if 0 != table & (1 << bit) {
                    tables.insert(TableOrdinal::new(table));
                }
            }
            presence.tables.insert(node, tables);
            nodes.push(NodePlan {
                resolved: Arc::new(ResolvedNode {
                    node,
                    node_type: NodeType::Integer,
                    access: NodeAccess::Value,
                }),
                outcomes,
                unrefined_outcomes: OutcomeSet::ALL,
                text_state: TextState::Settled,
            });
        }
        let leaf = LeafPlan {
            leaf: query.leaves()[0].clone(),
            nodes,
            text: TextMembers::default(),
            range: None,
            variable_ids: Lookup::NotRun,
            encoded_text: Lookup::NotRun,
        };
        (leaf, presence)
    }

    #[test]
    fn occurrences_are_always_true_only_where_no_other_value_is_possible() {
        let bits = |tables: &[u32]| {
            let mut set = TableBits::empty(4);
            for table in tables {
                set.insert(TableOrdinal::new(*table));
            }
            set
        };
        let outcomes = super::TableOutcomes {
            truthy: bits(&[0, 1, 2]),
            falsy: bits(&[1, 3]),
            unknown: bits(&[0, 3]),
        };
        let (may, must) = outcomes.occurrence(false);
        assert_eq!(
            may.iter().map(TableOrdinal::get).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(must.iter().map(TableOrdinal::get).collect::<Vec<_>>(), [2]);
        let (may, must) = outcomes.occurrence(true);
        assert_eq!(
            may.iter().map(TableOrdinal::get).collect::<Vec<_>>(),
            [1, 3]
        );
        assert_eq!(
            must.iter().map(TableOrdinal::get).collect::<Vec<_>>(),
            Vec::<u32>::new()
        );
        for (table, is_negated, expected) in [
            (0, false, OccurrenceState::Varies),
            (2, false, OccurrenceState::Always),
            (3, false, OccurrenceState::Never),
            (3, true, OccurrenceState::Varies),
            (2, true, OccurrenceState::Never),
        ] {
            assert_eq!(
                outcomes.state(TableOrdinal::new(table), is_negated),
                expected,
                "table {table}, negated {is_negated}"
            );
        }
    }

    #[test]
    fn table_outcomes_are_the_existential_join_of_the_present_nodes() {
        let mut num_checked = 0;
        for (kql, absent) in [("a: 1", TruthValue::Unknown), ("a: *", TruthValue::False)] {
            for first in SETS {
                for second in SETS {
                    for third in SETS {
                        let sets = [first, second, third];
                        let (leaf, presence) = leaf_plan(kql, sets);
                        let refined = leaf.table_outcomes(&presence, true);
                        for table in 0..NUM_TABLES {
                            let present = (0..3)
                                .filter(|bit| 0 != table & (1 << bit))
                                .map(|bit| sets[bit]);
                            let expected =
                                existential_join(present).unwrap_or_else(|| OutcomeSet::of(absent));
                            let ordinal = TableOrdinal::new(table);
                            let actual = [
                                (TruthValue::True, &refined.truthy),
                                (TruthValue::False, &refined.falsy),
                                (TruthValue::Unknown, &refined.unknown),
                            ]
                            .into_iter()
                            .filter(|(_, tables)| tables.contains(ordinal))
                            .fold(OutcomeSet::default(), |set, (value, _)| {
                                set.union(OutcomeSet::of(value))
                            });
                            assert_eq!(actual, expected, "{kql} {sets:?} table {table}");
                            num_checked += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(num_checked, 2 * 7 * 7 * 7 * NUM_TABLES);
    }
}
