//! The schema layer: the schema tree (MPT), the schema map and the table directory.

use std::collections::HashMap;
use std::collections::HashSet;
use std::ops::Range;

use crate::ByteRange;
use crate::ClassifiedVersion;
use crate::CorruptionReason;
use crate::DictionaryNeeds;
use crate::FormatError;
use crate::FormatLocation;
use crate::NodeId;
use crate::ReaderLimits;
use crate::SchemaId;
use crate::SectionId;
use crate::SharedBytes;
use crate::StreamId;
use crate::TableOrdinal;
use crate::UnsupportedFeature;
use crate::bytes_reader::ByteReader;

/// Storage node types; the numbering is frozen and append-only (Presto hard-codes 0-14).
#[non_exhaustive]
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NodeType {
    Integer = 0,
    Float = 1,
    ClpString = 2,
    VarString = 3,
    Boolean = 4,
    Object = 5,
    UnstructuredArray = 6,
    NullValue = 7,
    DeprecatedDateString = 8,
    StructuredArray = 9,
    Metadata = 10,
    DeltaInteger = 11,
    FormattedFloat = 12,
    DictionaryFloat = 13,
    Timestamp = 14,
}

impl NodeType {
    /// # Returns
    ///
    /// The node type numbered `value`, or `None` if the number is unknown.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Integer),
            1 => Some(Self::Float),
            2 => Some(Self::ClpString),
            3 => Some(Self::VarString),
            4 => Some(Self::Boolean),
            5 => Some(Self::Object),
            6 => Some(Self::UnstructuredArray),
            7 => Some(Self::NullValue),
            8 => Some(Self::DeprecatedDateString),
            9 => Some(Self::StructuredArray),
            10 => Some(Self::Metadata),
            11 => Some(Self::DeltaInteger),
            12 => Some(Self::FormattedFloat),
            13 => Some(Self::DictionaryFloat),
            14 => Some(Self::Timestamp),
            _ => None,
        }
    }

    /// # Returns
    ///
    /// Whether nodes of this type can have children in the schema tree.
    #[must_use]
    pub const fn is_container(self) -> bool {
        matches!(self, Self::Object | Self::Metadata | Self::StructuredArray)
    }

    /// # Returns
    ///
    /// The dictionaries a column of this type references (a format fact).
    #[must_use]
    pub const fn referenced_dictionaries(self) -> DictionaryNeeds {
        match self {
            Self::ClpString => DictionaryNeeds {
                is_variable_needed: true,
                is_logtype_needed: true,
                is_array_needed: false,
            },
            Self::UnstructuredArray => DictionaryNeeds {
                is_variable_needed: true,
                is_logtype_needed: false,
                is_array_needed: true,
            },
            Self::VarString | Self::DictionaryFloat => DictionaryNeeds {
                is_variable_needed: true,
                is_logtype_needed: false,
                is_array_needed: false,
            },
            Self::Integer
            | Self::Float
            | Self::Boolean
            | Self::Object
            | Self::NullValue
            | Self::DeprecatedDateString
            | Self::StructuredArray
            | Self::Metadata
            | Self::DeltaInteger
            | Self::FormattedFloat
            | Self::Timestamp => DictionaryNeeds {
                is_variable_needed: false,
                is_logtype_needed: false,
                is_array_needed: false,
            },
        }
    }
}

/// The validated schema tree (MPT).
///
/// Validation guarantees: every parent is the root or an earlier container node; `(parent, key,
/// type)` triples are unique; every node type is known; the depth is within
/// [`crate::ReaderLimits::max_schema_tree_depth`]; no node is a `StructuredArray` (rejected as
/// [`crate::UnsupportedFeature::StructuredArrays`] in this round).
#[derive(Debug)]
pub struct SchemaTree {
    /// The decoded section; node keys are slices of it.
    section: SharedBytes,
    nodes: Vec<StoredNode>,

    /// `children[child_offsets[i]..child_offsets[i + 1]]` are the children of node `i`.
    child_offsets: Vec<usize>,
    children: Vec<NodeId>,
    subtree_roots: Vec<NodeId>,
    log_event_idx_node: Option<NodeId>,
}

impl SchemaTree {
    /// # Returns
    ///
    /// The number of nodes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.nodes.len()
    }

    /// # Returns
    ///
    /// Whether the tree has no node.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// # Returns
    ///
    /// A view of node `id`, or `None` if it does not exist.
    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<SchemaNodeView<'_>> {
        let node = self.nodes.get(usize::try_from(id.get()).ok()?)?;
        Some(SchemaNodeView {
            id,
            parent: node.parent,
            key: &self.section[node.key.clone()],
            node_type: node.node_type,
        })
    }

    /// # Returns
    ///
    /// The children of node `id`, in id order (empty if `id` does not exist).
    #[must_use]
    pub fn children(&self, id: NodeId) -> &[NodeId] {
        let Ok(index) = usize::try_from(id.get()) else {
            return &[];
        };
        if index >= self.nodes.len() {
            return &[];
        }
        &self.children[self.child_offsets[index]..self.child_offsets[index + 1]]
    }

    /// # Returns
    ///
    /// The root of the subtree for `namespace` with root type `node_type` (e.g. `("", Object)` for
    /// records, `("@", Object)` for auto-generated keys, `("", Metadata)` for metadata), or `None`.
    #[must_use]
    pub fn subtree_root(&self, namespace: &[u8], node_type: NodeType) -> Option<NodeId> {
        self.subtree_roots.iter().copied().find(|root| {
            self.node(*root)
                .is_some_and(|view| view.key == namespace && view.node_type == node_type)
        })
    }

    /// # Returns
    ///
    /// The `log_event_idx` metadata node, or `None` if the archive does not record log order.
    #[must_use]
    pub const fn log_event_idx_node(&self) -> Option<NodeId> {
        self.log_event_idx_node
    }

    /// Factory function.
    ///
    /// Parses and validates the decoded `/schema_tree` section (`ReaderUtils::read_schema_tree`):
    /// a `u64` node count, then per node an `i32` parent id, a `u64` key length, the key and a
    /// `u8` node type.
    ///
    /// # Returns
    ///
    /// The validated tree on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if a parent is invalid or a triple repeats.
    /// * [`FormatError::LimitExceeded`] if the tree is deeper than
    ///   [`ReaderLimits::max_schema_tree_depth`].
    /// * Forwards [`ByteReader::read_u64`]'s return values on failure.
    /// * Forwards [`ByteReader::read_i32`]'s return values on failure.
    /// * Forwards [`ByteReader::read_bytes`]'s return values on failure.
    /// * Forwards [`ByteReader::check_count`]'s return values on failure.
    /// * Forwards [`read_node_type`]'s return values on failure.
    pub(crate) fn parse(
        section: SharedBytes,
        limits: &ReaderLimits,
        version: ClassifiedVersion,
    ) -> Result<Self, FormatError> {
        const MIN_NODE_SIZE: u64 = 4 + 8 + 1;

        let location = FormatLocation::Section(SectionId::SchemaTree);
        let mut reader = ByteReader::new(&section, location);
        let num_nodes = reader.read_u64()?;
        let num_nodes = reader.check_count(num_nodes, MIN_NODE_SIZE)?;
        FormatError::check_limit("schema tree node count", num_nodes as u64, i32::MAX as u64)?;

        let mut nodes: Vec<StoredNode> = Vec::with_capacity(num_nodes);
        let mut depths: Vec<u32> = Vec::with_capacity(num_nodes);
        let mut triples: HashSet<(Option<NodeId>, &[u8], NodeType)> =
            HashSet::with_capacity(num_nodes);
        let mut subtree_roots = Vec::new();
        for index in 0..num_nodes {
            let id = NodeId::new(u32::try_from(index).unwrap_or(u32::MAX));
            let invalid_parent = || {
                FormatError::corrupt(location, CorruptionReason::InvalidParent { node: id.get() })
            };
            let parent_id = reader.read_i32()?;
            let key_length = reader.read_u64()?;
            let key_begin = reader.position();
            let key = reader.read_bytes(key_length)?;
            let node_type = read_node_type(&mut reader, version)?;

            let (parent, depth) = if ROOT_PARENT_ID == parent_id {
                subtree_roots.push(id);
                (None, 0)
            } else {
                let parent_index = usize::try_from(parent_id)
                    .ok()
                    .filter(|parent_index| *parent_index < index)
                    .ok_or_else(invalid_parent)?;
                if !nodes[parent_index].node_type.is_container() {
                    return Err(invalid_parent());
                }
                let parent = NodeId::new(u32::try_from(parent_index).unwrap_or(u32::MAX));
                (Some(parent), depths[parent_index] + 1)
            };
            FormatError::check_limit(
                "schema tree depth",
                depth.into(),
                limits.max_schema_tree_depth.into(),
            )?;
            if !triples.insert((parent, key, node_type)) {
                return Err(FormatError::corrupt(
                    location,
                    CorruptionReason::DuplicateNode { node: id.get() },
                ));
            }
            nodes.push(StoredNode {
                parent,
                key: key_begin..key_begin + key.len(),
                node_type,
            });
            depths.push(depth);
        }
        drop(triples);

        let (child_offsets, children) = build_children(&nodes);
        let mut tree = Self {
            section,
            nodes,
            child_offsets,
            children,
            subtree_roots,
            log_event_idx_node: None,
        };
        tree.log_event_idx_node = tree
            .subtree_root(METADATA_NAMESPACE, NodeType::Metadata)
            .and_then(|metadata_root| {
                tree.children(metadata_root).iter().copied().find(|child| {
                    tree.node(*child)
                        .is_some_and(|view| view.key == LOG_EVENT_IDX_KEY)
                })
            });
        Ok(tree)
    }
}

/// A borrowed view of one schema-tree node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchemaNodeView<'tree> {
    pub id: NodeId,

    /// The parent, or `None` for a subtree root.
    pub parent: Option<NodeId>,

    pub key: &'tree [u8],
    pub node_type: NodeType,
}

/// All schemas of the archive.
#[derive(Debug)]
pub struct SchemaMap {
    nodes: Vec<NodeId>,
    schemas: HashMap<SchemaId, Range<usize>>,

    /// Schema ids in file order.
    ids: Vec<SchemaId>,
}

impl SchemaMap {
    /// # Returns
    ///
    /// Schema `id`, or `None` if it does not exist.
    #[must_use]
    pub fn schema(&self, id: SchemaId) -> Option<Schema<'_>> {
        let range = self.schemas.get(&id)?;
        Some(Schema {
            id,
            nodes: &self.nodes[range.clone()],
        })
    }

    /// # Returns
    ///
    /// The number of schemas.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.ids.len()
    }

    /// # Returns
    ///
    /// Whether the archive has no schema.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// # Returns
    ///
    /// The ids of every schema, in file order.
    #[must_use]
    pub fn ids(&self) -> &[SchemaId] {
        &self.ids
    }

    /// Factory function.
    ///
    /// Parses and validates the decoded `/schema_ids` section (`ReaderUtils::read_schemas`): a
    /// `u64` schema count, then per schema an `i32` id, a `u32` node count, a `u32` ordered node
    /// count and the `i32` node ids.
    ///
    /// # Returns
    ///
    /// The validated schemas on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if a schema id repeats, a schema declares more ordered nodes than
    ///   nodes, or a node id does not exist in `tree`.
    /// * [`FormatError::UnsupportedFeature`] if a schema has an unordered region (structured
    ///   arrays).
    /// * Forwards [`ByteReader::read_u64`]'s return values on failure.
    /// * Forwards [`ByteReader::read_i32`]'s return values on failure.
    /// * Forwards [`ByteReader::read_u32`]'s return values on failure.
    /// * Forwards [`ByteReader::check_count`]'s return values on failure.
    pub(crate) fn parse(section: &[u8], tree: &SchemaTree) -> Result<Self, FormatError> {
        const MIN_SCHEMA_SIZE: u64 = 4 + 4 + 4;
        const NODE_ID_SIZE: u64 = 4;

        let location = FormatLocation::Section(SectionId::SchemaMap);
        let mut reader = ByteReader::new(section, location);
        let num_schemas = reader.read_u64()?;
        let num_schemas = reader.check_count(num_schemas, MIN_SCHEMA_SIZE)?;

        let mut nodes = Vec::new();
        let mut schemas = HashMap::with_capacity(num_schemas);
        let mut ids = Vec::with_capacity(num_schemas);
        for _ in 0..num_schemas {
            let id = SchemaId::new(reader.read_i32()?);
            let num_nodes = reader.read_u32()?;
            let num_ordered_nodes = reader.read_u32()?;
            if num_ordered_nodes > num_nodes {
                return Err(reader.corrupt(CorruptionReason::InvalidSchema(id)));
            }
            if num_ordered_nodes < num_nodes {
                return Err(FormatError::UnsupportedFeature {
                    location,
                    feature: UnsupportedFeature::StructuredArrays,
                });
            }
            let num_nodes = reader.check_count(num_nodes.into(), NODE_ID_SIZE)?;
            let begin = nodes.len();
            nodes.reserve(num_nodes);
            for _ in 0..num_nodes {
                let node_id = reader.read_i32()?;
                let node = u32::try_from(node_id)
                    .ok()
                    .map(NodeId::new)
                    .filter(|node| tree.node(*node).is_some())
                    .ok_or_else(|| {
                        reader.corrupt(CorruptionReason::UnknownNode(NodeId::new(
                            node_id.cast_unsigned(),
                        )))
                    })?;
                nodes.push(node);
            }
            if schemas.insert(id, begin..nodes.len()).is_some() {
                return Err(reader.corrupt(CorruptionReason::DuplicateSchema(id)));
            }
            ids.push(id);
        }
        Ok(Self {
            nodes,
            schemas,
            ids,
        })
    }
}

/// One schema: its node ids in schema order (column order for the nodes that occupy bytes).
///
/// Schemas with an unordered region (structured arrays) are rejected at open in this round, so a
/// schema is exactly its ordered node list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Schema<'map> {
    pub id: SchemaId,
    pub nodes: &'map [NodeId],
}

/// One packed stream: a single zstd frame holding consecutive ERTs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackedStreamInfo {
    pub id: StreamId,

    /// The absolute range of the compressed frame in the archive file.
    pub compressed_range: ByteRange,

    pub uncompressed_size: u64,
}

/// One ERT: where its bytes are, how many rows it has, and its first record ordinal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableInfo {
    pub ordinal: TableOrdinal,
    pub schema_id: SchemaId,
    pub stream_id: StreamId,

    /// Offset of the ERT inside its decoded stream.
    pub stream_offset: u64,

    pub uncompressed_size: u64,
    pub num_rows: u32,

    /// Prefix sum of `num_rows` over the preceding tables in directory order; the basis of record
    /// ids for archives without log order.
    pub first_record_ordinal: u64,
}

/// The validated table metadata; the ERTs of each stream tile it exactly, in directory order.
#[derive(Debug)]
pub struct TableDirectory {
    tables: Vec<TableInfo>,
    streams: Vec<PackedStreamInfo>,
    num_log_events: u64,
}

impl TableDirectory {
    /// # Returns
    ///
    /// Every table in directory (writer) order.
    #[must_use]
    pub fn tables(&self) -> &[TableInfo] {
        &self.tables
    }

    /// # Returns
    ///
    /// Every packed stream in stream-id order.
    #[must_use]
    pub fn streams(&self) -> &[PackedStreamInfo] {
        &self.streams
    }

    /// # Returns
    ///
    /// Table `ordinal`, or `None` if it does not exist.
    #[must_use]
    pub fn table(&self, ordinal: TableOrdinal) -> Option<&TableInfo> {
        self.tables.get(usize::try_from(ordinal.get()).ok()?)
    }

    /// # Returns
    ///
    /// The total number of rows (log events) over every table.
    #[must_use]
    pub const fn num_log_events(&self) -> u64 {
        self.num_log_events
    }

    /// Factory function.
    ///
    /// Parses and validates the decoded `/table_metadata` section (`ArchiveReader::read_metadata`,
    /// `PackedStreamReader::read_metadata`): a `u64` stream count, per stream a `u64` offset into
    /// `packed_streams` and a `u64` decoded size; a `u64` count of separate column schemas; a
    /// `u64` table count, and per table a `u64` stream id, a `u64` stream offset, an `i32` schema
    /// id and a `u64` row count. A table ends where the next table of its stream starts, or at the
    /// end of the stream. Unlike C++, an archive without tables is accepted (DV-30).
    ///
    /// # Returns
    ///
    /// The validated directory on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if a stream range is invalid, a table references a missing stream
    ///   or schema, a schema has two tables, or the tables do not tile their streams.
    /// * [`FormatError::UnsupportedFeature`] if the archive has separate column schemas or a table
    ///   has more than `u32::MAX` rows.
    /// * Forwards [`ByteReader::read_u64`]'s return values on failure.
    /// * Forwards [`ByteReader::read_i32`]'s return values on failure.
    /// * Forwards [`ByteReader::check_count`]'s return values on failure.
    /// * Forwards [`parse_streams`]'s return values on failure.
    pub(crate) fn parse(
        section: &[u8],
        packed_streams: ByteRange,
        schema_map: &SchemaMap,
    ) -> Result<Self, FormatError> {
        const TABLE_SIZE: u64 = 8 + 8 + 4 + 8;

        let location = FormatLocation::Section(SectionId::TableMetadata);
        let mut reader = ByteReader::new(section, location);
        let streams = parse_streams(&mut reader, packed_streams)?;

        let num_separate_column_schemas = reader.read_u64()?;
        if 0 != num_separate_column_schemas {
            return Err(FormatError::UnsupportedFeature {
                location,
                feature: UnsupportedFeature::SeparateColumnSchemas,
            });
        }

        let num_tables = reader.read_u64()?;
        let num_tables = reader.check_count(num_tables, TABLE_SIZE)?;
        FormatError::check_limit("table count", num_tables as u64, u32::MAX.into())?;
        let mut tables: Vec<TableInfo> = Vec::with_capacity(num_tables);
        let mut schema_ids = HashSet::with_capacity(num_tables);
        let mut num_log_events: u64 = 0;
        for index in 0..num_tables {
            let ordinal = TableOrdinal::new(u32::try_from(index).unwrap_or(u32::MAX));
            let tiling_error = || {
                FormatError::corrupt(
                    FormatLocation::Table(ordinal),
                    CorruptionReason::TableTiling,
                )
            };
            let stream_index = reader.read_u64()?;
            let stream_offset = reader.read_u64()?;
            let schema_id = SchemaId::new(reader.read_i32()?);
            let num_rows = reader.read_u64()?;

            let stream = usize::try_from(stream_index)
                .ok()
                .and_then(|stream_index| streams.get(stream_index))
                .ok_or_else(|| {
                    FormatError::corrupt(
                        FormatLocation::Table(ordinal),
                        CorruptionReason::UnknownStream(stream_index),
                    )
                })?;
            if schema_map.schema(schema_id).is_none() {
                return Err(FormatError::corrupt(
                    FormatLocation::Table(ordinal),
                    CorruptionReason::UnknownSchema(schema_id),
                ));
            }
            if !schema_ids.insert(schema_id) {
                return Err(FormatError::corrupt(
                    FormatLocation::Table(ordinal),
                    CorruptionReason::DuplicateTable(schema_id),
                ));
            }
            let num_rows =
                u32::try_from(num_rows).map_err(|_| FormatError::UnsupportedFeature {
                    location: FormatLocation::Table(ordinal),
                    feature: UnsupportedFeature::TableRowCount(num_rows),
                })?;

            match tables.last_mut() {
                Some(previous) if previous.stream_id == stream.id => {
                    if stream_offset < previous.stream_offset {
                        return Err(tiling_error());
                    }
                    previous.uncompressed_size = stream_offset - previous.stream_offset;
                }
                previous => {
                    let expected_stream_index =
                        previous.map_or(0, |previous| previous.stream_id.get() + 1);
                    if stream.id.get() != expected_stream_index || 0 != stream_offset {
                        return Err(tiling_error());
                    }
                }
            }
            if stream_offset > stream.uncompressed_size {
                return Err(tiling_error());
            }
            tables.push(TableInfo {
                ordinal,
                schema_id,
                stream_id: stream.id,
                stream_offset,
                uncompressed_size: stream.uncompressed_size - stream_offset,
                num_rows,
                first_record_ordinal: num_log_events,
            });
            num_log_events += u64::from(num_rows);
        }
        let num_covered_streams = tables
            .last()
            .map_or(0, |last| last.stream_id.get() as usize + 1);
        if num_covered_streams != streams.len() {
            return Err(reader.corrupt(CorruptionReason::TableTiling));
        }

        Ok(Self {
            tables,
            streams,
            num_log_events,
        })
    }
}

/// Query-independent inverted index from a schema-tree node to the tables whose schema contains
/// it.
#[derive(Debug)]
pub struct NodeTableIndex {
    /// `tables[offsets[i]..offsets[i + 1]]` are the tables of node `i`.
    offsets: Vec<usize>,
    tables: Vec<TableOrdinal>,
}

impl NodeTableIndex {
    /// # Returns
    ///
    /// The tables whose schema contains `node`, in ordinal order.
    #[must_use]
    pub fn tables_for_node(&self, node: NodeId) -> &[TableOrdinal] {
        let Ok(index) = usize::try_from(node.get()) else {
            return &[];
        };
        if index + 1 >= self.offsets.len() {
            return &[];
        }
        &self.tables[self.offsets[index]..self.offsets[index + 1]]
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The inverted index over the validated `tree`, `schema_map` and `directory`.
    pub(crate) fn build(
        tree: &SchemaTree,
        schema_map: &SchemaMap,
        directory: &TableDirectory,
    ) -> Self {
        let table_nodes = |table: &TableInfo| {
            schema_map
                .schema(table.schema_id)
                .map_or(&[][..], |schema| schema.nodes)
        };
        let mut last_tables: Vec<Option<TableOrdinal>> = vec![None; tree.len()];
        let mut offsets = vec![0_usize; tree.len() + 1];
        for table in directory.tables() {
            for node in table_nodes(table) {
                let index = node.get() as usize;
                if last_tables[index] != Some(table.ordinal) {
                    last_tables[index] = Some(table.ordinal);
                    offsets[index + 1] += 1;
                }
            }
        }
        for index in 0..tree.len() {
            offsets[index + 1] += offsets[index];
        }
        let mut next_slots = offsets.clone();
        let mut tables = vec![TableOrdinal::new(0); offsets[tree.len()]];
        last_tables.fill(None);
        for table in directory.tables() {
            for node in table_nodes(table) {
                let index = node.get() as usize;
                if last_tables[index] != Some(table.ordinal) {
                    last_tables[index] = Some(table.ordinal);
                    tables[next_slots[index]] = table.ordinal;
                    next_slots[index] += 1;
                }
            }
        }
        Self { offsets, tables }
    }
}

const LOG_EVENT_IDX_KEY: &[u8] = b"log_event_idx";
const METADATA_NAMESPACE: &[u8] = b"";
const ROOT_PARENT_ID: i32 = -1;

/// One parsed schema-tree node.
#[derive(Debug)]
struct StoredNode {
    parent: Option<NodeId>,

    /// The key's range in the decoded section.
    key: Range<usize>,

    node_type: NodeType,
}

/// Reads a node type byte.
///
/// # Returns
///
/// The node type on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`FormatError::Corrupt`] if the type is unknown in a known patch release.
/// * [`FormatError::UnsupportedFeature`] if the node is a structured array, or the type is unknown
///   in a newer patch release.
/// * Forwards [`ByteReader::read_u8`]'s return values on failure.
fn read_node_type(
    reader: &mut ByteReader<'_>,
    version: ClassifiedVersion,
) -> Result<NodeType, FormatError> {
    let type_number = reader.read_u8()?;
    match NodeType::from_u8(type_number) {
        Some(NodeType::StructuredArray) => Err(FormatError::UnsupportedFeature {
            location: reader.location(),
            feature: UnsupportedFeature::StructuredArrays,
        }),
        Some(node_type) => Ok(node_type),
        None if version.is_newer_patch => Err(FormatError::UnsupportedFeature {
            location: reader.location(),
            feature: UnsupportedFeature::NewerNodeType(type_number),
        }),
        None => Err(reader.corrupt(CorruptionReason::UnknownNodeType(type_number))),
    }
}

/// Groups node ids by parent.
///
/// # Returns
///
/// A tuple of:
///
/// * The offsets: the children of node `i` are `children[offsets[i]..offsets[i + 1]]`.
/// * The children, grouped by parent and in id order within each group.
fn build_children(nodes: &[StoredNode]) -> (Vec<usize>, Vec<NodeId>) {
    let mut offsets = vec![0_usize; nodes.len() + 1];
    for node in nodes {
        if let Some(parent) = node.parent {
            offsets[parent.get() as usize + 1] += 1;
        }
    }
    for index in 0..nodes.len() {
        offsets[index + 1] += offsets[index];
    }
    let mut next_slots = offsets.clone();
    let mut children = vec![NodeId::new(0); offsets[nodes.len()]];
    for (id, node) in (0_u32..).zip(nodes) {
        if let Some(parent) = node.parent {
            let slot = &mut next_slots[parent.get() as usize];
            children[*slot] = NodeId::new(id);
            *slot += 1;
        }
    }
    (offsets, children)
}

/// Reads the packed-stream directory: a `u64` count and per stream a `u64` offset into
/// `packed_streams` and a `u64` decoded size. As in C++, a stream's frame ends where the next one
/// starts, and the last one at the end of the packed-stream section.
///
/// # Returns
///
/// The streams on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`FormatError::Corrupt`] if the offsets decrease or leave the packed-stream section.
/// * [`FormatError::LimitExceeded`] if there are more than `u32::MAX` streams.
/// * Forwards [`ByteReader::read_u64`]'s return values on failure.
/// * Forwards [`ByteReader::check_count`]'s return values on failure.
fn parse_streams(
    reader: &mut ByteReader<'_>,
    packed_streams: ByteRange,
) -> Result<Vec<PackedStreamInfo>, FormatError> {
    const STREAM_SIZE: u64 = 8 + 8;

    let num_streams = reader.read_u64()?;
    let num_streams = reader.check_count(num_streams, STREAM_SIZE)?;
    FormatError::check_limit("packed stream count", num_streams as u64, u32::MAX.into())?;
    let mut stream_offsets = Vec::with_capacity(num_streams);
    for _ in 0..num_streams {
        let file_offset = reader.read_u64()?;
        let uncompressed_size = reader.read_u64()?;
        stream_offsets.push((file_offset, uncompressed_size));
    }

    let mut streams = Vec::with_capacity(num_streams);
    for (id, (index, (file_offset, uncompressed_size))) in
        (0_u32..).zip(stream_offsets.iter().enumerate())
    {
        let begin = packed_streams.begin.checked_add(*file_offset);
        let end = match stream_offsets.get(index + 1) {
            Some((next_offset, _)) => packed_streams.begin.checked_add(*next_offset),
            None => Some(packed_streams.end),
        };
        let range = begin
            .zip(end)
            .filter(|(begin, end)| begin <= end && *end <= packed_streams.end)
            .ok_or_else(|| reader.corrupt(CorruptionReason::InvalidStreamDirectory))?;
        streams.push(PackedStreamInfo {
            id: StreamId::new(id),
            compressed_range: ByteRange {
                begin: range.0,
                end: range.1,
            },
            uncompressed_size: *uncompressed_size,
        });
    }
    Ok(streams)
}

#[cfg(test)]
mod tests {
    use super::NodeType;
    use super::SchemaMap;
    use super::SchemaTree;
    use super::TableDirectory;
    use crate::ByteRange;
    use crate::ClassifiedVersion;
    use crate::CorruptionReason;
    use crate::FormatError;
    use crate::FormatVersion;
    use crate::NodeId;
    use crate::ReaderLimits;
    use crate::SchemaId;
    use crate::SharedBytes;
    use crate::StreamId;
    use crate::TableOrdinal;
    use crate::UnsupportedFeature;

    type SchemaSpec<'nodes> = (i32, u32, &'nodes [i32]);
    type StreamSpec = (u64, u64);
    type TableSpec = (u64, u64, i32, u64);

    const OBJECT: u8 = NodeType::Object as u8;
    const INTEGER: u8 = NodeType::Integer as u8;
    const METADATA: u8 = NodeType::Metadata as u8;
    const PACKED_STREAMS: ByteRange = ByteRange {
        begin: 1000,
        end: 1300,
    };

    /// # Returns
    ///
    /// The classified version 0.5.`patch`.
    fn version(patch: u16) -> ClassifiedVersion {
        ClassifiedVersion::classify(FormatVersion {
            major: 0,
            minor: 5,
            patch,
        })
        .expect("0.5.x is in the window")
    }

    /// # Returns
    ///
    /// The decoded `/schema_tree` section of `(parent, key, type)` nodes.
    fn encode_tree(nodes: &[(i32, &[u8], u8)]) -> SharedBytes {
        let mut bytes = (nodes.len() as u64).to_le_bytes().to_vec();
        for (parent, key, node_type) in nodes {
            bytes.extend_from_slice(&parent.to_le_bytes());
            bytes.extend_from_slice(&(key.len() as u64).to_le_bytes());
            bytes.extend_from_slice(key);
            bytes.push(*node_type);
        }
        SharedBytes::from(bytes)
    }

    /// # Returns
    ///
    /// The tree of `nodes` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`SchemaTree::parse`]'s return values on failure.
    fn parse_tree(nodes: &[(i32, &[u8], u8)]) -> Result<SchemaTree, FormatError> {
        SchemaTree::parse(encode_tree(nodes), &ReaderLimits::default(), version(1))
    }

    /// # Returns
    ///
    /// The decoded `/schema_ids` section of `(id, ordered count, nodes)` schemas.
    fn encode_map(schemas: &[SchemaSpec<'_>]) -> Vec<u8> {
        let mut bytes = (schemas.len() as u64).to_le_bytes().to_vec();
        for (id, num_ordered, nodes) in schemas {
            bytes.extend_from_slice(&id.to_le_bytes());
            bytes.extend_from_slice(&u32::try_from(nodes.len()).expect("few nodes").to_le_bytes());
            bytes.extend_from_slice(&num_ordered.to_le_bytes());
            for node in *nodes {
                bytes.extend_from_slice(&node.to_le_bytes());
            }
        }
        bytes
    }

    /// # Returns
    ///
    /// The decoded `/table_metadata` section of `streams` and `tables`.
    fn encode_directory(streams: &[StreamSpec], tables: &[TableSpec]) -> Vec<u8> {
        let mut bytes = (streams.len() as u64).to_le_bytes().to_vec();
        for (offset, size) in streams {
            bytes.extend_from_slice(&offset.to_le_bytes());
            bytes.extend_from_slice(&size.to_le_bytes());
        }
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        bytes.extend_from_slice(&(tables.len() as u64).to_le_bytes());
        for (stream, offset, schema, num_rows) in tables {
            bytes.extend_from_slice(&stream.to_le_bytes());
            bytes.extend_from_slice(&offset.to_le_bytes());
            bytes.extend_from_slice(&schema.to_le_bytes());
            bytes.extend_from_slice(&num_rows.to_le_bytes());
        }
        bytes
    }

    /// # Returns
    ///
    /// A valid tree with records, a nested object and a `log_event_idx` node.
    fn sample_tree() -> SchemaTree {
        parse_tree(&[
            (-1, b"", OBJECT),
            (0, b"a", INTEGER),
            (0, b"b", OBJECT),
            (2, b"c", INTEGER),
            (-1, b"", METADATA),
            (4, b"log_event_idx", INTEGER),
            (0, b"a", OBJECT),
        ])
        .expect("the sample tree is valid")
    }

    /// # Returns
    ///
    /// Three valid schemas over `tree`.
    fn sample_map(tree: &SchemaTree) -> SchemaMap {
        SchemaMap::parse(
            &encode_map(&[(0, 2, &[1, 5]), (7, 2, &[3, 5]), (3, 0, &[])]),
            tree,
        )
        .expect("the sample map is valid")
    }

    /// # Returns
    ///
    /// The corruption reason of `error`.
    fn corruption_reason(error: FormatError) -> CorruptionReason {
        match error {
            FormatError::Corrupt { reason, .. } => *reason,
            other => panic!("expected a corruption error, got {other:?}"),
        }
    }

    #[test]
    fn node_type_numbering_is_frozen() {
        const FROZEN: [NodeType; 15] = [
            NodeType::Integer,
            NodeType::Float,
            NodeType::ClpString,
            NodeType::VarString,
            NodeType::Boolean,
            NodeType::Object,
            NodeType::UnstructuredArray,
            NodeType::NullValue,
            NodeType::DeprecatedDateString,
            NodeType::StructuredArray,
            NodeType::Metadata,
            NodeType::DeltaInteger,
            NodeType::FormattedFloat,
            NodeType::DictionaryFloat,
            NodeType::Timestamp,
        ];
        for (number, node_type) in (0_u8..).zip(FROZEN) {
            assert_eq!(node_type as u8, number);
            assert_eq!(NodeType::from_u8(number), Some(node_type));
        }
        assert_eq!(NodeType::from_u8(15), None);
        assert_eq!(NodeType::from_u8(u8::MAX), None);
    }

    #[test]
    fn only_dictionary_backed_types_reference_dictionaries() {
        let clp_string = NodeType::ClpString.referenced_dictionaries();
        assert!(clp_string.is_variable_needed && clp_string.is_logtype_needed);
        assert!(!clp_string.is_array_needed);

        let array = NodeType::UnstructuredArray.referenced_dictionaries();
        assert!(array.is_variable_needed && array.is_array_needed);
        assert!(!array.is_logtype_needed);

        for node_type in [NodeType::VarString, NodeType::DictionaryFloat] {
            let needs = node_type.referenced_dictionaries();
            assert!(
                needs.is_variable_needed,
                "{node_type:?} reads the variable dictionary"
            );
            assert!(!needs.is_logtype_needed && !needs.is_array_needed);
        }

        for node_type in [
            NodeType::Integer,
            NodeType::DeltaInteger,
            NodeType::Float,
            NodeType::FormattedFloat,
            NodeType::Boolean,
            NodeType::Timestamp,
            NodeType::Object,
            NodeType::NullValue,
        ] {
            assert_eq!(
                node_type.referenced_dictionaries(),
                crate::DictionaryNeeds::default(),
                "{node_type:?} reads no dictionary"
            );
        }
    }

    #[test]
    fn parses_a_tree_with_children_subtrees_and_log_order() {
        let tree = sample_tree();
        assert_eq!(tree.len(), 7);
        assert!(!tree.is_empty());
        let node = tree.node(NodeId::new(3)).expect("node 3 exists");
        assert_eq!(node.parent, Some(NodeId::new(2)));
        assert_eq!(node.key, b"c");
        assert_eq!(node.node_type, NodeType::Integer);
        assert_eq!(
            tree.node(NodeId::new(0)).map(|root| root.parent),
            Some(None)
        );
        assert_eq!(tree.node(NodeId::new(7)), None);

        assert_eq!(
            tree.children(NodeId::new(0)),
            [1, 2, 6].map(NodeId::new).as_slice()
        );
        assert_eq!(tree.children(NodeId::new(1)), &[]);
        assert_eq!(tree.children(NodeId::new(99)), &[]);
        assert_eq!(
            tree.subtree_root(b"", NodeType::Object),
            Some(NodeId::new(0))
        );
        assert_eq!(
            tree.subtree_root(b"", NodeType::Metadata),
            Some(NodeId::new(4))
        );
        assert_eq!(tree.subtree_root(b"@", NodeType::Object), None);
        assert_eq!(tree.log_event_idx_node(), Some(NodeId::new(5)));
    }

    #[test]
    fn archives_without_a_metadata_subtree_have_no_log_order() -> anyhow::Result<()> {
        let tree = parse_tree(&[(-1, b"", OBJECT), (0, b"log_event_idx", INTEGER)])?;
        assert_eq!(tree.log_event_idx_node(), None);
        let empty = parse_tree(&[])?;
        assert!(empty.is_empty(), "{empty:?}");
        assert_eq!(empty.subtree_root(b"", NodeType::Object), None);
        Ok(())
    }

    #[test]
    fn rejects_invalid_parents() {
        for nodes in [
            &[(0, &b"self"[..], OBJECT)][..],
            &[(1, b"forward", OBJECT), (-1, b"", OBJECT)],
            &[(-2, b"negative", OBJECT)],
            &[
                (-1, b"", OBJECT),
                (0, b"leaf", INTEGER),
                (1, b"child", INTEGER),
            ],
        ] {
            let error = parse_tree(nodes).expect_err("the parent is invalid");
            assert!(
                matches!(
                    corruption_reason(error),
                    CorruptionReason::InvalidParent { .. }
                ),
                "{nodes:?}"
            );
        }
    }

    #[test]
    fn rejects_duplicate_triples_but_not_shared_keys() -> anyhow::Result<()> {
        let error = parse_tree(&[(-1, b"", OBJECT), (0, b"a", INTEGER), (0, b"a", INTEGER)])
            .expect_err("a duplicate triple shifts node ids in C++");
        assert_eq!(
            corruption_reason(error),
            CorruptionReason::DuplicateNode { node: 2 }
        );
        parse_tree(&[(-1, b"", OBJECT), (0, b"a", INTEGER), (0, b"a", OBJECT)])?;
        Ok(())
    }

    #[test]
    fn rejects_structured_arrays_and_unknown_node_types() {
        let error = parse_tree(&[
            (-1, b"", OBJECT),
            (0, b"a", NodeType::StructuredArray as u8),
        ])
        .expect_err("structured arrays are out of scope");
        assert!(
            matches!(
                error,
                FormatError::UnsupportedFeature {
                    feature: UnsupportedFeature::StructuredArrays,
                    ..
                }
            ),
            "{error:?}"
        );

        let error = parse_tree(&[(-1, b"", 15)]).expect_err("type 15 is unknown");
        assert_eq!(
            corruption_reason(error),
            CorruptionReason::UnknownNodeType(15)
        );

        let error = SchemaTree::parse(
            encode_tree(&[(-1, b"", 15)]),
            &ReaderLimits::default(),
            version(2),
        )
        .expect_err("type 15 is unknown");
        assert!(
            matches!(
                error,
                FormatError::UnsupportedFeature {
                    feature: UnsupportedFeature::NewerNodeType(15),
                    ..
                }
            ),
            "a newer patch may add node types: {error:?}"
        );
    }

    #[test]
    fn enforces_the_depth_limit() -> anyhow::Result<()> {
        let limits = ReaderLimits {
            max_schema_tree_depth: 2,
            ..ReaderLimits::default()
        };
        let chain = |depth: i32| -> Vec<(i32, &'static [u8], u8)> {
            (0..=depth)
                .map(|index| (index - 1, &b"k"[..], OBJECT))
                .collect()
        };
        SchemaTree::parse(encode_tree(&chain(2)), &limits, version(1))?;
        let error = SchemaTree::parse(encode_tree(&chain(3)), &limits, version(1))
            .expect_err("depth 3 exceeds the limit");
        assert!(
            matches!(
                error,
                FormatError::LimitExceeded {
                    actual_size: 3,
                    limit_size: 2,
                    ..
                }
            ),
            "{error:?}"
        );
        Ok(())
    }

    #[test]
    fn rejects_truncated_trees_and_huge_counts() {
        let encoded = encode_tree(&[(-1, b"", OBJECT), (0, b"key", INTEGER)]);
        for length in 0..encoded.len() {
            assert!(
                SchemaTree::parse(
                    encoded.slice(..length),
                    &ReaderLimits::default(),
                    version(1)
                )
                .is_err(),
                "a tree truncated to {length} bytes must be rejected"
            );
        }
        let mut huge = encoded.to_vec();
        huge[..8].copy_from_slice(&u64::MAX.to_le_bytes());
        let error = SchemaTree::parse(
            SharedBytes::from(huge),
            &ReaderLimits::default(),
            version(1),
        )
        .expect_err("the count cannot fit");
        assert!(matches!(
            corruption_reason(error),
            CorruptionReason::CountExceedsRemaining { .. }
        ));
    }

    #[test]
    fn parses_schemas_in_file_order() {
        let tree = sample_tree();
        let map = sample_map(&tree);
        assert_eq!(map.len(), 3);
        assert!(!map.is_empty());
        assert_eq!(map.ids(), [0, 7, 3].map(SchemaId::new).as_slice());
        let schema = map.schema(SchemaId::new(7)).expect("schema 7 exists");
        assert_eq!(schema.nodes, [3, 5].map(NodeId::new).as_slice());
        assert_eq!(
            map.schema(SchemaId::new(3)).map(|schema| schema.nodes),
            Some(&[][..])
        );
        assert_eq!(map.schema(SchemaId::new(1)), None);
    }

    #[test]
    fn rejects_invalid_schemas() {
        let tree = sample_tree();
        let cases: [(&[SchemaSpec<'_>], CorruptionReason); 4] = [
            (
                &[(0, 1, &[9])],
                CorruptionReason::UnknownNode(NodeId::new(9)),
            ),
            (
                &[(0, 1, &[-1])],
                CorruptionReason::UnknownNode(NodeId::new(u32::MAX)),
            ),
            (
                &[(0, 1, &[1]), (0, 1, &[2])],
                CorruptionReason::DuplicateSchema(SchemaId::new(0)),
            ),
            (
                &[(4, 2, &[1])],
                CorruptionReason::InvalidSchema(SchemaId::new(4)),
            ),
        ];
        for (schemas, expected_reason) in cases {
            let error = SchemaMap::parse(&encode_map(schemas), &tree)
                .expect_err("the schema map is invalid");
            assert_eq!(corruption_reason(error), expected_reason);
        }

        let error = SchemaMap::parse(&encode_map(&[(0, 1, &[1, 3])]), &tree)
            .expect_err("an unordered region means structured arrays");
        assert!(
            matches!(
                error,
                FormatError::UnsupportedFeature {
                    feature: UnsupportedFeature::StructuredArrays,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn parses_a_directory_and_derives_table_sizes() -> anyhow::Result<()> {
        let tree = sample_tree();
        let map = sample_map(&tree);
        let directory = TableDirectory::parse(
            &encode_directory(
                &[(0, 100), (120, 50)],
                &[(0, 0, 7, 3), (0, 60, 0, 2), (1, 0, 3, 5)],
            ),
            PACKED_STREAMS,
            &map,
        )?;
        assert_eq!(directory.num_log_events(), 10);
        let streams = directory.streams();
        assert_eq!(streams.len(), 2);
        assert_eq!(
            streams[0].compressed_range,
            ByteRange {
                begin: 1000,
                end: 1120,
            }
        );
        assert_eq!(
            streams[1].compressed_range,
            ByteRange {
                begin: 1120,
                end: 1300,
            }
        );
        let sizes: Vec<_> = directory
            .tables()
            .iter()
            .map(|table| {
                (
                    table.stream_id,
                    table.stream_offset,
                    table.uncompressed_size,
                    table.first_record_ordinal,
                )
            })
            .collect();
        assert_eq!(
            sizes,
            vec![
                (StreamId::new(0), 0, 60, 0),
                (StreamId::new(0), 60, 40, 3),
                (StreamId::new(1), 0, 50, 5),
            ]
        );
        let table = directory
            .table(TableOrdinal::new(1))
            .expect("table 1 exists");
        assert_eq!(table.schema_id, SchemaId::new(0));
        assert_eq!(table.num_rows, 2);
        assert_eq!(directory.table(TableOrdinal::new(3)), None);
        Ok(())
    }

    #[test]
    fn accepts_an_empty_directory() -> anyhow::Result<()> {
        let tree = sample_tree();
        let directory = TableDirectory::parse(
            &encode_directory(&[], &[]),
            PACKED_STREAMS,
            &sample_map(&tree),
        )?;
        assert_eq!(directory.tables(), &[]);
        assert_eq!(directory.streams(), &[]);
        assert_eq!(directory.num_log_events(), 0);
        Ok(())
    }

    #[test]
    fn rejects_directories_that_do_not_tile() {
        let tree = sample_tree();
        let map = sample_map(&tree);
        let cases: [(&[StreamSpec], &[TableSpec], CorruptionReason); 9] = [
            (&[(0, 100)], &[(0, 10, 0, 1)], CorruptionReason::TableTiling),
            (
                &[(0, 100)],
                &[(0, 0, 0, 1), (0, 101, 7, 1)],
                CorruptionReason::TableTiling,
            ),
            (
                &[(0, 100)],
                &[(0, 50, 0, 1), (0, 0, 7, 1)],
                CorruptionReason::TableTiling,
            ),
            (
                &[(0, 10), (5, 10)],
                &[(1, 0, 0, 1), (0, 0, 7, 1)],
                CorruptionReason::TableTiling,
            ),
            (
                &[(0, 10), (5, 10)],
                &[(0, 0, 0, 1)],
                CorruptionReason::TableTiling,
            ),
            (
                &[(0, 10)],
                &[(1, 0, 0, 1)],
                CorruptionReason::UnknownStream(1),
            ),
            (
                &[(0, 10)],
                &[(0, 0, 5, 1)],
                CorruptionReason::UnknownSchema(SchemaId::new(5)),
            ),
            (
                &[(0, 10)],
                &[(0, 0, 0, 1), (0, 5, 0, 1)],
                CorruptionReason::DuplicateTable(SchemaId::new(0)),
            ),
            (
                &[(50, 10), (10, 10)],
                &[(0, 0, 0, 1), (1, 0, 7, 1)],
                CorruptionReason::InvalidStreamDirectory,
            ),
        ];
        for (streams, tables, expected_reason) in cases {
            let error =
                TableDirectory::parse(&encode_directory(streams, tables), PACKED_STREAMS, &map)
                    .expect_err("the directory is invalid");
            assert_eq!(
                corruption_reason(error),
                expected_reason,
                "streams {streams:?}, tables {tables:?}"
            );
        }
    }

    #[test]
    fn rejects_unsupported_directories() {
        let tree = sample_tree();
        let map = sample_map(&tree);
        let error = TableDirectory::parse(
            &encode_directory(&[(0, 10)], &[(0, 0, 0, u64::from(u32::MAX) + 1)]),
            PACKED_STREAMS,
            &map,
        )
        .expect_err("a table row count must fit in u32");
        assert!(
            matches!(
                error,
                FormatError::UnsupportedFeature {
                    feature: UnsupportedFeature::TableRowCount(4_294_967_296),
                    ..
                }
            ),
            "{error:?}"
        );

        let mut separate_columns = encode_directory(&[], &[]);
        separate_columns[8..16].copy_from_slice(&1_u64.to_le_bytes());
        let error = TableDirectory::parse(&separate_columns, PACKED_STREAMS, &map)
            .expect_err("separate column schemas are unsupported");
        assert!(
            matches!(
                error,
                FormatError::UnsupportedFeature {
                    feature: UnsupportedFeature::SeparateColumnSchemas,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn indexes_tables_by_node() -> anyhow::Result<()> {
        let tree = sample_tree();
        let map = SchemaMap::parse(&encode_map(&[(0, 3, &[1, 5, 1]), (7, 2, &[3, 5])]), &tree)?;
        let directory = TableDirectory::parse(
            &encode_directory(&[(0, 100)], &[(0, 0, 7, 1), (0, 10, 0, 1)]),
            PACKED_STREAMS,
            &map,
        )?;
        let index = super::NodeTableIndex::build(&tree, &map, &directory);
        assert_eq!(
            index.tables_for_node(NodeId::new(5)),
            [0, 1].map(TableOrdinal::new).as_slice()
        );
        assert_eq!(
            index.tables_for_node(NodeId::new(1)),
            [TableOrdinal::new(1)].as_slice()
        );
        assert_eq!(index.tables_for_node(NodeId::new(0)), &[]);
        assert_eq!(index.tables_for_node(NodeId::new(100)), &[]);
        Ok(())
    }
}
