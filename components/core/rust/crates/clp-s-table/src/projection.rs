//! Resolved, semantics-free projections.

use clp_s_format::NodeId;

use crate::ColumnSet;
use crate::TableLayout;

/// Which leaves of each matched record are rendered, resolved against one archive's schema tree
/// (C++ `search/Projection.cpp`: literal paths through objects to leaf nodes only).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ResolvedProjection {
    /// Every leaf.
    #[default]
    Full,

    /// Only these leaf nodes, sorted by id; a record with none of them renders as `{}`.
    Nodes(Box<[NodeId]>),
}

impl ResolvedProjection {
    /// # Returns
    ///
    /// Whether leaf `node` is rendered.
    #[must_use]
    pub fn includes(&self, node: NodeId) -> bool {
        match self {
            Self::Full => true,
            Self::Nodes(nodes) => nodes.binary_search(&node).is_ok(),
        }
    }

    /// # Returns
    ///
    /// The columns of `layout` that rendering the projected records reads.
    #[must_use]
    pub fn output_columns(&self, layout: &TableLayout) -> ColumnSet {
        match self {
            Self::Full => ColumnSet::full(layout.num_columns()),
            Self::Nodes(nodes) => {
                let mut columns = ColumnSet::empty(layout.num_columns());
                for node in nodes {
                    for column in layout.columns_for_node(*node) {
                        columns.insert(*column);
                    }
                }
                columns
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_projections_include_every_node() {
        let projection = ResolvedProjection::default();
        assert!(projection.includes(NodeId::new(0)));
        assert!(projection.includes(NodeId::new(u32::MAX)));
    }

    #[test]
    fn node_projections_include_only_their_nodes() {
        let projection =
            ResolvedProjection::Nodes(vec![NodeId::new(2), NodeId::new(5), NodeId::new(9)].into());
        for (node, is_included) in [(0, false), (2, true), (5, true), (6, false), (9, true)] {
            assert_eq!(
                projection.includes(NodeId::new(node)),
                is_included,
                "node {node}"
            );
        }
    }
}
