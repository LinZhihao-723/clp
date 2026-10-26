//! Compiled table layouts and record shapes.

use std::collections::HashMap;

use clp_s_format::ArchiveIndex;
use clp_s_format::NodeId;
use clp_s_format::NodeType;
use clp_s_format::SchemaId;
use clp_s_format::SchemaTree;
use clp_s_format::TableInfo;

use crate::ColumnEncoding;
use crate::ColumnIndex;
use crate::LayoutError;
use crate::ResolvedProjection;
use crate::TableError;
use crate::table::ColumnData;

/// One column of a table layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColumnSpec {
    pub node_id: NodeId,
    pub node_type: NodeType,
    pub encoding: ColumnEncoding,
}

/// How many leading bytes of an ERT its first columns take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeadingColumnsSize {
    /// The columns end at this offset.
    Known(u64),

    /// The size depends on bytes past the given prefix: decode at least this many leading bytes
    /// and measure again.
    AtLeast(u64),
}

/// The compiled, validated layout shared by every ERT of one schema in one archive.
///
/// Compiling a layout is the semantic validation tier: the schema's nodes must exist, be listed in
/// strictly increasing id order (which rejects the duplicate-key case B4), and have node types
/// this round supports.
#[derive(Debug)]
pub struct TableLayout {
    schema_id: SchemaId,
    columns: Box<[ColumnSpec]>,

    /// `column_indices[i]` is `ColumnIndex(i)`, so [`Self::columns_for_node`] can return a slice.
    column_indices: Box<[ColumnIndex]>,

    log_event_idx_column: Option<ColumnIndex>,
    record_timestamp_column: Option<ColumnIndex>,
    shape: RecordShape,
}

impl TableLayout {
    /// # Returns
    ///
    /// The schema this layout was compiled from.
    #[must_use]
    pub const fn schema_id(&self) -> SchemaId {
        self.schema_id
    }

    /// # Returns
    ///
    /// The columns in buffer order; [`ColumnIndex`] `i` is element `i`.
    #[must_use]
    pub fn columns(&self) -> &[ColumnSpec] {
        &self.columns
    }

    /// # Returns
    ///
    /// The number of columns.
    #[must_use]
    pub fn num_columns(&self) -> u32 {
        u32::try_from(self.columns.len()).unwrap_or(u32::MAX)
    }

    /// # Returns
    ///
    /// The columns bound to `node_id`, in buffer order (at most one in this round, since
    /// structured arrays are rejected).
    #[must_use]
    pub fn columns_for_node(&self, node_id: NodeId) -> &[ColumnIndex] {
        self.columns
            .binary_search_by_key(&node_id, |spec| spec.node_id)
            .map_or(&[], |position| &self.column_indices[position..=position])
    }

    /// # Returns
    ///
    /// The `log_event_idx` column, or `None` if the archive does not record log order.
    #[must_use]
    pub const fn log_event_idx_column(&self) -> Option<ColumnIndex> {
        self.log_event_idx_column
    }

    /// # Returns
    ///
    /// The column of the archive's authoritative timestamp node, or `None` if this table has none.
    #[must_use]
    pub const fn record_timestamp_column(&self) -> Option<ColumnIndex> {
        self.record_timestamp_column
    }

    /// # Returns
    ///
    /// The shape of the records of this table in the default namespace.
    #[must_use]
    pub const fn shape(&self) -> &RecordShape {
        &self.shape
    }

    /// Measures the leading `num_columns` columns of the ERT `info` of this layout from `prefix`,
    /// its decoded leading bytes.
    ///
    /// # Returns
    ///
    /// The size of the columns on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`ColumnData::measure`]'s return values on failure.
    pub fn leading_columns_size(
        &self,
        info: &TableInfo,
        prefix: &[u8],
        num_columns: usize,
    ) -> Result<LeadingColumnsSize, TableError> {
        ColumnData::measure(
            self.columns
                .iter()
                .take(num_columns)
                .map(|spec| spec.encoding),
            prefix,
            info.num_rows,
            info.uncompressed_size,
            info.ordinal,
        )
    }

    /// Builds the shape of the records of this layout rendered under `projection`: like
    /// [`Self::shape`], but only the schema nodes the projection includes are inserted (after their
    /// missing ancestors), so a record with none of them renders as `{}`.
    ///
    /// With [`ResolvedProjection::Full`], the result equals [`Self::shape`].
    ///
    /// # Returns
    ///
    /// The projected shape on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`LayoutError::UnknownSchema`] if this layout's schema does not exist in `index`.
    pub fn projected_shape(
        &self,
        index: &ArchiveIndex,
        projection: &ResolvedProjection,
    ) -> Result<RecordShape, LayoutError> {
        let schema = index
            .schema_map()
            .schema(self.schema_id)
            .ok_or(LayoutError::UnknownSchema(self.schema_id))?;
        Ok(RecordShape::build(
            index.schema_tree(),
            schema
                .nodes
                .iter()
                .copied()
                .filter(|node| projection.includes(*node)),
            &self.columns,
        ))
    }

    /// # Returns
    ///
    /// The spec of `column`, or `None` if it is outside the layout.
    pub(crate) fn column(&self, column: ColumnIndex) -> Option<&ColumnSpec> {
        self.columns.get(column.get() as usize)
    }

    /// Factory function.
    ///
    /// Compiles the layout of schema `schema_id` of the archive with `index`: one column per
    /// schema node that occupies bytes, in schema order, plus the record shape.
    ///
    /// # Returns
    ///
    /// The layout on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`LayoutError::UnknownSchema`] if the schema does not exist.
    /// * [`LayoutError::UnknownNode`] if the schema references a missing node.
    /// * [`LayoutError::NodeOrder`] if the schema's node ids are not strictly increasing.
    /// * [`LayoutError::UnsupportedNodeType`] if a node's type is not supported in a schema.
    /// * [`LayoutError::LogEventIdxNodeType`] if the schema holds the `log_event_idx` node and its
    ///   type is neither `Integer` nor `DeltaInteger`.
    pub(crate) fn compile(index: &ArchiveIndex, schema_id: SchemaId) -> Result<Self, LayoutError> {
        // The opener already rejects schemas and nodes that do not exist; these checks keep the
        // layout sound for any index it is given.
        let schema = index
            .schema_map()
            .schema(schema_id)
            .ok_or(LayoutError::UnknownSchema(schema_id))?;
        let tree = index.schema_tree();
        let log_event_idx_node = tree.log_event_idx_node();

        let mut columns = Vec::new();
        let mut previous_node: Option<NodeId> = None;
        for node_id in schema.nodes.iter().copied() {
            if previous_node.is_some_and(|previous| previous >= node_id) {
                return Err(LayoutError::NodeOrder {
                    schema: schema_id,
                    node: node_id,
                });
            }
            previous_node = Some(node_id);
            let node = tree
                .node(node_id)
                .ok_or(LayoutError::UnknownNode(node_id))?;
            if Some(node_id) == log_event_idx_node
                && !matches!(node.node_type, NodeType::Integer | NodeType::DeltaInteger)
            {
                return Err(LayoutError::LogEventIdxNodeType {
                    node: node_id,
                    node_type: node.node_type,
                });
            }
            let encoding =
                ColumnEncoding::of_node_type(node.node_type).map_err(|error| match error {
                    LayoutError::UnsupportedNodeType { node_type, .. } => {
                        LayoutError::UnsupportedNodeType {
                            node: Some(node_id),
                            node_type,
                        }
                    }
                    error => error,
                })?;
            if let Some(encoding) = encoding {
                columns.push(ColumnSpec {
                    node_id,
                    node_type: node.node_type,
                    encoding,
                });
            }
        }

        let column_by_node: HashMap<NodeId, ColumnIndex> = (0_u32..)
            .zip(&columns)
            .map(|(index, spec)| (spec.node_id, ColumnIndex::new(index)))
            .collect();
        let log_event_idx_column =
            log_event_idx_node.and_then(|node| column_by_node.get(&node).copied());
        let record_timestamp_column = index
            .metadata()
            .timestamp_index()
            .authoritative()
            .and_then(|entry| column_by_node.get(&entry.node_id).copied())
            .filter(|column| ColumnEncoding::Timestamp == columns[column.get() as usize].encoding);
        let shape = RecordShape::build(tree, schema.nodes.iter().copied(), &columns);
        let column_indices = (0..u32::try_from(columns.len()).unwrap_or(u32::MAX))
            .map(ColumnIndex::new)
            .collect();
        Ok(Self {
            schema_id,
            columns: columns.into_boxed_slice(),
            column_indices,
            log_event_idx_column,
            record_timestamp_column,
            shape,
        })
    }
}

/// The shape of a table's records in the default (`""`) namespace, as a flat sequence of
/// structural events in C++ key order (the local-tree insertion order of `SchemaReader`).
///
/// Auto-generated (`@`) keys and metadata columns are not part of the shape.
#[derive(Debug, PartialEq, Eq)]
pub struct RecordShape {
    events: Box<[ShapeEvent]>,
}

impl RecordShape {
    /// # Returns
    ///
    /// The events of the shape, starting with the root [`ShapeEvent::BeginObject`] and ending
    /// with its [`ShapeEvent::EndObject`].
    #[must_use]
    pub fn events(&self) -> &[ShapeEvent] {
        &self.events
    }

    /// Factory function.
    ///
    /// Builds the shape the way `SchemaReader::initialize_serializer` builds its local tree: every
    /// node of `schema_nodes` under the default-namespace root is inserted, after its missing
    /// ancestors, in order, so each object lists its children in first-insertion order. Objects
    /// without a child in the local tree are empty objects.
    ///
    /// # Type Parameters
    ///
    /// * `NodesType` - Iterates over the schema nodes to insert, in schema order.
    ///
    /// # Returns
    ///
    /// The shape of the records holding `schema_nodes`, whose leaves are stored in `columns`.
    fn build<NodesType: Iterator<Item = NodeId>>(
        tree: &SchemaTree,
        schema_nodes: NodesType,
        columns: &[ColumnSpec],
    ) -> Self {
        let mut events = vec![ShapeEvent::BeginObject { node_id: None }];
        if let Some(root) = tree.subtree_root(b"", NodeType::Object) {
            let local_tree = LocalTree::build(tree, root, schema_nodes);
            local_tree.emit(tree, columns, &mut events);
        }
        events.push(ShapeEvent::EndObject);
        Self {
            events: events.into_boxed_slice(),
        }
    }
}

/// One structural event of a [`RecordShape`]; keys come from the schema tree by node id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShapeEvent {
    /// Opens an object: the record root (`node_id` is `None`) or the object stored under the key
    /// of `node_id`.
    BeginObject { node_id: Option<NodeId> },

    /// Closes the most recently opened object.
    EndObject,

    /// A leaf whose value is stored in `column`.
    Column {
        node_id: NodeId,
        column: ColumnIndex,
    },

    /// A `null` leaf (a `NullValue` node, which occupies no bytes).
    Null { node_id: NodeId },

    /// An empty object (an `Object` node with no descendant in this schema).
    EmptyObject { node_id: NodeId },
}

/// The local schema tree of one schema under the default-namespace root, as `SchemaReader`
/// builds it.
struct LocalTree {
    /// Local node `0` is the root; every other entry is a global node and its local children.
    nodes: Vec<LocalNode>,
}

impl LocalTree {
    /// Factory function.
    ///
    /// # Type Parameters
    ///
    /// * `NodesType` - Iterates over the schema nodes to insert, in schema order.
    ///
    /// # Returns
    ///
    /// The local tree of the nodes of `schema_nodes` that lie under `root`.
    fn build<NodesType: Iterator<Item = NodeId>>(
        tree: &SchemaTree,
        root: NodeId,
        schema_nodes: NodesType,
    ) -> Self {
        let mut nodes = vec![LocalNode {
            node_id: root,
            children: Vec::new(),
        }];
        let mut local_by_node: HashMap<NodeId, usize> = HashMap::from([(root, 0)]);
        let mut path = Vec::new();
        for node_id in schema_nodes {
            path.clear();
            let mut current = node_id;
            let attach_point = loop {
                if let Some(local) = local_by_node.get(&current) {
                    break Some(*local);
                }
                path.push(current);
                match tree.node(current).and_then(|node| node.parent) {
                    Some(parent) => current = parent,
                    None => break None,
                }
            };
            let Some(mut parent_local) = attach_point else {
                continue;
            };
            for node_id in path.iter().rev().copied() {
                let local = nodes.len();
                nodes.push(LocalNode {
                    node_id,
                    children: Vec::new(),
                });
                nodes[parent_local].children.push(local);
                local_by_node.insert(node_id, local);
                parent_local = local;
            }
        }
        Self { nodes }
    }

    /// Appends the events of every descendant of the root to `events`, depth first in local child
    /// order; a leaf is a column event if `columns` (sorted by node id) stores it.
    fn emit(&self, tree: &SchemaTree, columns: &[ColumnSpec], events: &mut Vec<ShapeEvent>) {
        let column_of = |node_id: NodeId| {
            columns
                .binary_search_by_key(&node_id, |spec| spec.node_id)
                .ok()
                .and_then(|position| u32::try_from(position).ok())
                .map(ColumnIndex::new)
        };
        let mut stack = vec![Frame {
            local: 0,
            next_child: 0,
        }];
        while let Some(frame) = stack.pop() {
            let Some(&child) = self.nodes[frame.local].children.get(frame.next_child) else {
                if !stack.is_empty() {
                    events.push(ShapeEvent::EndObject);
                }
                continue;
            };
            stack.push(Frame {
                next_child: frame.next_child + 1,
                ..frame
            });
            let child_node = &self.nodes[child];
            let node_id = child_node.node_id;
            let Some(node_type) = tree.node(node_id).map(|node| node.node_type) else {
                continue;
            };
            if let Some(column) = column_of(node_id) {
                events.push(ShapeEvent::Column { node_id, column });
                continue;
            }
            match node_type {
                NodeType::Object if child_node.children.is_empty() => {
                    events.push(ShapeEvent::EmptyObject { node_id });
                }
                NodeType::Object => {
                    events.push(ShapeEvent::BeginObject {
                        node_id: Some(node_id),
                    });
                    stack.push(Frame {
                        local: child,
                        next_child: 0,
                    });
                }
                NodeType::NullValue => events.push(ShapeEvent::Null { node_id }),
                _ => {}
            }
        }
    }
}

/// One node of a [`LocalTree`].
struct LocalNode {
    node_id: NodeId,
    children: Vec<usize>,
}

/// An object of a [`LocalTree`] being emitted, and the position of its next child.
#[derive(Clone, Copy)]
struct Frame {
    local: usize,
    next_child: usize,
}
