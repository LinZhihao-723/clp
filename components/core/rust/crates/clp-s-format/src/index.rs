//! The archive index: metadata plus the validated schema layer.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;

use crate::ArchiveMetadata;
use crate::DictionaryKind;
use crate::DictionaryNeeds;
use crate::NodeTableIndex;
use crate::ReadPlan;
use crate::SchemaMap;
use crate::SchemaTree;
use crate::SectionId;
use crate::StreamRead;
use crate::TableDirectory;
use crate::TableSet;

/// Metadata plus the validated schema layer; decoded once and immutable.
#[derive(Debug)]
pub struct ArchiveIndex {
    metadata: Arc<ArchiveMetadata>,
    schema_tree: SchemaTree,
    schema_map: SchemaMap,
    tables: TableDirectory,
    node_tables: OnceLock<NodeTableIndex>,
}

impl ArchiveIndex {
    /// # Returns
    ///
    /// The archive metadata.
    #[must_use]
    pub const fn metadata(&self) -> &Arc<ArchiveMetadata> {
        &self.metadata
    }

    /// # Returns
    ///
    /// The schema tree.
    #[must_use]
    pub const fn schema_tree(&self) -> &SchemaTree {
        &self.schema_tree
    }

    /// # Returns
    ///
    /// The schema map.
    #[must_use]
    pub const fn schema_map(&self) -> &SchemaMap {
        &self.schema_map
    }

    /// # Returns
    ///
    /// The table directory.
    #[must_use]
    pub const fn tables(&self) -> &TableDirectory {
        &self.tables
    }

    /// Returns the node-to-tables inverted index, building it on first use.
    ///
    /// # Returns
    ///
    /// The inverted index.
    #[must_use]
    pub fn node_tables(&self) -> &NodeTableIndex {
        self.node_tables.get_or_init(|| {
            NodeTableIndex::build(&self.schema_tree, &self.schema_map, &self.tables)
        })
    }

    /// # Returns
    ///
    /// Whether the archive records log order (has a `log_event_idx` node).
    #[must_use]
    pub const fn has_log_order(&self) -> bool {
        self.schema_tree.log_event_idx_node().is_some()
    }

    /// Plans the reads for `tables` and `dictionaries`.
    ///
    /// # Returns
    ///
    /// The plan, with streams in ascending stream order, each decoded only up to the end of its
    /// last needed table.
    #[must_use]
    pub fn plan_reads(&self, tables: &TableSet, dictionaries: DictionaryNeeds) -> ReadPlan {
        let sections = self.metadata.sections();
        let dictionary_ranges = [
            (
                dictionaries.is_variable_needed,
                DictionaryKind::Variable,
                SectionId::VariableDictionary,
            ),
            (
                dictionaries.is_logtype_needed,
                DictionaryKind::LogType,
                SectionId::LogTypeDictionary,
            ),
            (
                dictionaries.is_array_needed,
                DictionaryKind::Array,
                SectionId::ArrayDictionary,
            ),
        ]
        .into_iter()
        .filter(|(is_needed, ..)| *is_needed)
        .filter_map(|(_, kind, section)| Some((kind, sections.range(section)?)))
        .collect::<HashMap<_, _>>();

        let mut streams: Vec<StreamRead> = Vec::new();
        for table in self.tables.tables() {
            if !tables.contains(table.ordinal) {
                continue;
            }
            let table_end = table.stream_offset + table.uncompressed_size;
            match streams.last_mut() {
                Some(read) if read.stream.id == table.stream_id => {
                    read.decoded_prefix_size = read.decoded_prefix_size.max(table_end);
                    read.tables.push(table.ordinal);
                }
                _ => {
                    let Some(stream) = self.tables.streams().get(table.stream_id.get() as usize)
                    else {
                        continue;
                    };
                    streams.push(StreamRead {
                        stream: *stream,
                        decoded_prefix_size: table_end,
                        tables: vec![table.ordinal],
                    });
                }
            }
        }
        ReadPlan {
            dictionary_ranges,
            streams,
        }
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The index over the validated parts.
    pub(crate) const fn new(
        metadata: Arc<ArchiveMetadata>,
        schema_tree: SchemaTree,
        schema_map: SchemaMap,
        tables: TableDirectory,
    ) -> Self {
        Self {
            metadata,
            schema_tree,
            schema_map,
            tables,
            node_tables: OnceLock::new(),
        }
    }
}
