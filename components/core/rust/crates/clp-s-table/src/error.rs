//! Errors of the table layer.

use clp_s_encoding::EncodingError;
use clp_s_format::DictionaryKind;
use clp_s_format::FormatError;
use clp_s_format::NodeId;
use clp_s_format::NodeType;
use clp_s_format::SchemaId;
use clp_s_format::TableOrdinal;
use clp_s_resources::ErrorKind;

use crate::ColumnIndex;

/// Errors raised while compiling a table layout (the semantic validation tier).
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    #[error("table {0:?} does not exist")]
    UnknownTable(TableOrdinal),

    #[error("schema {0:?} of the table directory does not exist")]
    UnknownSchema(SchemaId),

    #[error("node {0:?} of a schema does not exist")]
    UnknownNode(NodeId),

    #[error("schema {schema:?} lists node {node:?} out of order (duplicate keys)")]
    NodeOrder { schema: SchemaId, node: NodeId },

    /// `node` is `None` when the error comes from [`crate::ColumnEncoding::of_node_type`], which
    /// knows only the node type.
    #[error("node {node:?} has type {node_type:?}, which this reader does not support in a schema")]
    UnsupportedNodeType {
        node: Option<NodeId>,
        node_type: NodeType,
    },

    #[error("the log_event_idx node {node:?} has type {node_type:?}, which is not an integer type")]
    LogEventIdxNodeType { node: NodeId, node_type: NodeType },
}

impl LayoutError {
    /// # Returns
    ///
    /// The category this error maps onto.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        match self {
            Self::UnknownTable(_) => ErrorKind::Internal,
            Self::UnknownSchema(_)
            | Self::UnknownNode(_)
            | Self::NodeOrder { .. }
            | Self::LogEventIdxNodeType { .. } => ErrorKind::CorruptArchive,
            Self::UnsupportedNodeType { .. } => ErrorKind::UnsupportedArchive,
        }
    }
}

/// Errors raised while loading or scanning a table.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum TableError {
    #[error(transparent)]
    Layout(#[from] LayoutError),

    #[error(transparent)]
    Format(#[from] FormatError),

    #[error(transparent)]
    Encoding(#[from] EncodingError),

    #[error("the columns of table {0:?} do not tile its bytes")]
    ColumnTiling(TableOrdinal),

    #[error("row {row} of table {table:?} references variables out of range")]
    VariableOutOfRange { table: TableOrdinal, row: u32 },

    #[error("invalid unstructured array: {reason}")]
    InvalidArray { reason: &'static str },

    #[error("column {0:?} is not decoded in this window")]
    ColumnNotDecoded(ColumnIndex),

    #[error("column {0:?} does not hold integers")]
    NotIntegerColumn(ColumnIndex),

    #[error("the {0:?} dictionary is not loaded")]
    DictionaryNotLoaded(DictionaryKind),
}

impl TableError {
    /// # Returns
    ///
    /// The category this error maps onto.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        match self {
            Self::Layout(error) => error.kind(),
            Self::Format(error) => error.kind(),
            Self::Encoding(_)
            | Self::ColumnTiling(_)
            | Self::VariableOutOfRange { .. }
            | Self::InvalidArray { .. } => ErrorKind::CorruptArchive,
            Self::ColumnNotDecoded(_)
            | Self::NotIntegerColumn(_)
            | Self::DictionaryNotLoaded(_) => ErrorKind::Internal,
        }
    }
}
