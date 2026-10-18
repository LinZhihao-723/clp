//! Immutable archive plans.

use std::ops::Range;
use std::sync::Arc;

use clp_s_format::DictionaryNeeds;
use clp_s_format::NodeId;
use clp_s_format::TableOrdinal;
use clp_s_query::LeafId;
use clp_s_query::NormalExpr;
use clp_s_query::Query;
use clp_s_query::TextPattern;
use clp_s_table::ArchiveTables;
use clp_s_table::ColumnIndex;
use clp_s_table::ResolvedProjection;

use crate::ArrayAccess;
use crate::EncodedTextMatch;
use crate::IdSet;

/// The immutable plan of one query on one archive, shared by every scan of it.
#[derive(Debug)]
pub struct ArchiveSearchPlan {
    archive_tables: Arc<ArchiveTables>,
    query: Arc<Query>,
    tables: Box<[TableSearchPlan]>,
    templates: Box<[Arc<ResidualTemplate>]>,
    projection: ResolvedProjection,
    filter_dictionary_needs: DictionaryNeeds,
}

impl ArchiveSearchPlan {
    /// # Returns
    ///
    /// The archive's table layer.
    #[must_use]
    pub const fn archive_tables(&self) -> &Arc<ArchiveTables> {
        &self.archive_tables
    }

    /// # Returns
    ///
    /// The query the plan evaluates; residual templates and bindings refer to its leaves.
    #[must_use]
    pub const fn query(&self) -> &Arc<Query> {
        &self.query
    }

    /// # Returns
    ///
    /// The candidate tables, in table-directory order.
    #[must_use]
    pub fn tables(&self) -> &[TableSearchPlan] {
        &self.tables
    }

    /// # Returns
    ///
    /// The distinct residual templates, indexed by [`ResidualTemplate::index`].
    #[must_use]
    pub fn templates(&self) -> &[Arc<ResidualTemplate>] {
        &self.templates
    }

    /// # Returns
    ///
    /// The projection resolved against the archive's schema tree.
    #[must_use]
    pub const fn projection(&self) -> &ResolvedProjection {
        &self.projection
    }

    /// # Returns
    ///
    /// The dictionaries the residual filters read (output needs are derived from the scan's
    /// column demand).
    #[must_use]
    pub const fn filter_dictionary_needs(&self) -> DictionaryNeeds {
        self.filter_dictionary_needs
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The plan made of the given parts.
    pub(crate) const fn new(
        archive_tables: Arc<ArchiveTables>,
        query: Arc<Query>,
        tables: Box<[TableSearchPlan]>,
        templates: Box<[Arc<ResidualTemplate>]>,
        projection: ResolvedProjection,
        filter_dictionary_needs: DictionaryNeeds,
    ) -> Self {
        Self {
            archive_tables,
            query,
            tables,
            templates,
            projection,
            filter_dictionary_needs,
        }
    }
}

/// The plan of one candidate table.
#[derive(Debug)]
pub struct TableSearchPlan {
    pub table: TableOrdinal,
    pub num_rows: u32,
    pub filter: TableFilterPlan,
}

/// How the rows of one candidate table are selected.
#[derive(Debug)]
pub enum TableFilterPlan {
    /// Every row matches; no filter column is decoded.
    MatchAll,

    /// Rows are selected by a residual filter; compile it with
    /// [`crate::CompiledTableFilter::compile`].
    Residual(ResidualFilter),
}

/// The residual filter of one table: a shared template plus this table's column bindings.
///
/// A row matches iff the template root is True on it, where a positive leaf occurrence is True
/// iff the leaf is True on the row, and a negated occurrence is True iff the leaf is False (an
/// Unknown leaf selects neither). The leaf's value on a row is the existential join of its
/// binding: True if some bound column is True, else False if some bound column is False or the
/// binding [`LeafBinding::is_false_without_columns`], else Unknown.
#[derive(Debug)]
pub struct ResidualFilter {
    template: Arc<ResidualTemplate>,

    /// Sorted by leaf id; one per leaf of the template.
    bindings: Box<[LeafBinding]>,
}

impl ResidualFilter {
    /// # Returns
    ///
    /// The shared template.
    #[must_use]
    pub const fn template(&self) -> &Arc<ResidualTemplate> {
        &self.template
    }

    /// # Returns
    ///
    /// The root of the template: an NNF tree of leaf occurrences, never a constant.
    #[must_use]
    pub fn root(&self) -> &NormalExpr {
        self.template.root()
    }

    /// # Returns
    ///
    /// The bindings of the template's leaves to this table, sorted by leaf id.
    #[must_use]
    pub fn bindings(&self) -> &[LeafBinding] {
        &self.bindings
    }

    /// # Returns
    ///
    /// The binding of `leaf` to this table, or `None` if `leaf` is not in the template.
    #[must_use]
    pub fn binding(&self, leaf: LeafId) -> Option<&LeafBinding> {
        self.bindings
            .binary_search_by_key(&leaf, |binding| binding.leaf)
            .ok()
            .map(|position| &self.bindings[position])
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The residual of `template` with `bindings`, which it sorts by leaf id.
    pub(crate) fn new(template: Arc<ResidualTemplate>, mut bindings: Vec<LeafBinding>) -> Self {
        bindings.sort_by_key(|binding| binding.leaf);
        Self {
            template,
            bindings: bindings.into_boxed_slice(),
        }
    }
}

/// A folded residual shared by every table with the same leaf states.
#[derive(Debug, PartialEq, Eq)]
pub struct ResidualTemplate {
    index: usize,
    root: NormalExpr,
}

impl ResidualTemplate {
    /// # Returns
    ///
    /// The position of the template in [`ArchiveSearchPlan::templates`].
    #[must_use]
    pub const fn index(&self) -> usize {
        self.index
    }

    /// # Returns
    ///
    /// The root: an NNF tree whose leaves are the query's leaves, never a constant.
    #[must_use]
    pub const fn root(&self) -> &NormalExpr {
        &self.root
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The template at `index` with `root`.
    pub(crate) const fn new(index: usize, root: NormalExpr) -> Self {
        Self { index, root }
    }
}

/// The columns of one table a leaf reads.
#[derive(Debug)]
pub struct LeafBinding {
    pub leaf: LeafId,

    /// The bound columns, in column order; several columns are an existential join.
    pub columns: Box<[BoundColumn]>,

    /// Whether some value the leaf reaches in every row of the table is False without reading a
    /// column (a `null` the test rejects, or a column proven False by the dictionaries or the
    /// storage rule), so the leaf is False on every row where no bound column is True.
    pub is_false_without_columns: bool,
}

/// One column a leaf reads, and how to test it.
#[derive(Debug)]
pub struct BoundColumn {
    pub column: ColumnIndex,
    pub node_id: NodeId,
    pub test: ColumnTest,
}

/// How a bound column is tested against its leaf.
#[derive(Debug)]
pub enum ColumnTest {
    /// Compare each value with the leaf's test (integers, floats including dictionary floats,
    /// booleans and timestamps); the column is comparable, so every row is True or False.
    Value,

    /// A `VarString` value is True iff its variable id is in the set, else False.
    VariableIds(Arc<IdSet>),

    /// A `ClpString` value is True iff it matches some member (see [`EncodedTextMember`]), else
    /// False.
    EncodedText(Arc<[EncodedTextMember]>),

    /// The values below the row's unstructured array reached by the access, each tested like a
    /// scalar; a row reaching no comparable value is Unknown (`Exists` is True iff some value is
    /// reached).
    Array(ArrayAccess),

    /// The `log_event_idx` column: a row's value is the outcome of the range-index range holding
    /// its `log_event_idx`.
    LogEventRanges(Arc<RangeOutcomes>),
}

/// One text pattern a `ClpString` column is matched against, with its dictionary prefilter.
#[derive(Debug, PartialEq, Eq)]
pub struct EncodedTextMember {
    pub pattern: TextPattern,
    pub prefilter: EncodedTextMatch,
}

/// The outcome of a range-index leaf per `log_event_idx` range.
///
/// A row is True if its `log_event_idx` lies in a true range, False if it lies in a false range,
/// and otherwise (no range holds it, so it has no range-index field) False if
/// [`Self::is_false_outside_ranges`], else Unknown.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RangeOutcomes {
    /// Sorted, disjoint, non-adjacent ranges.
    pub true_ranges: Box<[Range<u64>]>,

    /// Sorted, disjoint, non-adjacent ranges.
    pub false_ranges: Box<[Range<u64>]>,

    /// Whether the leaf is False on a record without range-index fields (an `Exists` test).
    pub is_false_outside_ranges: bool,
}

impl RangeOutcomes {
    /// # Returns
    ///
    /// The outcome of the row with `log_event_idx`: `Some(true)` for True, `Some(false)` for
    /// False and `None` for Unknown.
    #[must_use]
    pub fn outcome(&self, log_event_idx: u64) -> Option<bool> {
        let contains = |ranges: &[Range<u64>]| {
            let position = ranges.partition_point(|range| range.end <= log_event_idx);
            ranges
                .get(position)
                .is_some_and(|range| range.start <= log_event_idx)
        };
        if contains(&self.true_ranges) {
            Some(true)
        } else if contains(&self.false_ranges) || self.is_false_outside_ranges {
            Some(false)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use super::RangeOutcomes;

    #[test]
    fn range_outcomes_look_up_the_containing_range() {
        let outcomes = RangeOutcomes {
            true_ranges: vec![0..3, 10..12].into_boxed_slice(),
            false_ranges: vec![Range { start: 3, end: 5 }].into_boxed_slice(),
            is_false_outside_ranges: false,
        };
        for (log_event_idx, expected) in [
            (0, Some(true)),
            (2, Some(true)),
            (3, Some(false)),
            (4, Some(false)),
            (5, None),
            (9, None),
            (10, Some(true)),
            (12, None),
            (u64::MAX, None),
        ] {
            assert_eq!(outcomes.outcome(log_event_idx), expected, "{log_event_idx}");
        }
        let exists = RangeOutcomes {
            is_false_outside_ranges: true,
            ..outcomes
        };
        assert_eq!(exists.outcome(0), Some(true));
        assert_eq!(exists.outcome(7), Some(false));
    }
}
