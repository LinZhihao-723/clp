//! The physical encoding of ERT columns.

use clp_s_format::NodeType;

use crate::LayoutError;

/// The physical encoding of one ERT column; [`ColumnEncoding::of_node_type`] is the only mapping
/// from [`NodeType`] to bytes.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ColumnEncoding {
    /// `i64` values.
    Int64,

    /// `i64` deltas, prefix-summed per window.
    DeltaInt64,

    /// `f64` values.
    Float64,

    /// `f64` values plus a `u16` float format each.
    FormattedFloat64,

    /// `u64` variable-dictionary ids whose entries are float texts.
    DictionaryFloat,

    /// One byte per value.
    Boolean,

    /// `u64` logtype words plus a variable region (see [`crate::EncodedTextBatch`]).
    ClpString,

    /// `u64` variable-dictionary ids.
    VariableString,

    /// `ClpString`-encoded JSON arrays whose logtypes live in the array dictionary.
    UnstructuredArray,

    /// Delta-encoded `i64` epoch nanoseconds plus a `u64` timestamp pattern id each.
    Timestamp,
}

impl ColumnEncoding {
    /// Maps a node type to its encoding.
    ///
    /// # Returns
    ///
    /// The encoding, or `None` for node types that occupy no bytes (objects, nulls, metadata
    /// roots), on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`LayoutError::UnsupportedNodeType`] if the node type cannot appear in a 0.5.x schema
    ///   readable by this round (`DeprecatedDateString`, `StructuredArray`); the error names no
    ///   node.
    pub const fn of_node_type(node_type: NodeType) -> Result<Option<Self>, LayoutError> {
        match node_type {
            NodeType::Integer => Ok(Some(Self::Int64)),
            NodeType::DeltaInteger => Ok(Some(Self::DeltaInt64)),
            NodeType::Float => Ok(Some(Self::Float64)),
            NodeType::FormattedFloat => Ok(Some(Self::FormattedFloat64)),
            NodeType::DictionaryFloat => Ok(Some(Self::DictionaryFloat)),
            NodeType::Boolean => Ok(Some(Self::Boolean)),
            NodeType::ClpString => Ok(Some(Self::ClpString)),
            NodeType::VarString => Ok(Some(Self::VariableString)),
            NodeType::UnstructuredArray => Ok(Some(Self::UnstructuredArray)),
            NodeType::Timestamp => Ok(Some(Self::Timestamp)),
            NodeType::Object | NodeType::NullValue | NodeType::Metadata => Ok(None),
            // Node types added by newer patch releases fail at open; none occupies a column here.
            NodeType::DeprecatedDateString | NodeType::StructuredArray | _ => {
                Err(LayoutError::UnsupportedNodeType {
                    node: None,
                    node_type,
                })
            }
        }
    }

    /// # Returns
    ///
    /// Whether the column's `i64` values are stored as deltas and prefix-summed per window.
    #[must_use]
    pub const fn is_delta_encoded(self) -> bool {
        matches!(self, Self::DeltaInt64 | Self::Timestamp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_node_type_maps_to_its_cpp_column_reader() {
        let expected = [
            (NodeType::Integer, Some(ColumnEncoding::Int64)),
            (NodeType::DeltaInteger, Some(ColumnEncoding::DeltaInt64)),
            (NodeType::Float, Some(ColumnEncoding::Float64)),
            (
                NodeType::FormattedFloat,
                Some(ColumnEncoding::FormattedFloat64),
            ),
            (
                NodeType::DictionaryFloat,
                Some(ColumnEncoding::DictionaryFloat),
            ),
            (NodeType::Boolean, Some(ColumnEncoding::Boolean)),
            (NodeType::ClpString, Some(ColumnEncoding::ClpString)),
            (NodeType::VarString, Some(ColumnEncoding::VariableString)),
            (
                NodeType::UnstructuredArray,
                Some(ColumnEncoding::UnstructuredArray),
            ),
            (NodeType::Timestamp, Some(ColumnEncoding::Timestamp)),
            (NodeType::Object, None),
            (NodeType::NullValue, None),
            (NodeType::Metadata, None),
        ];
        for (node_type, encoding) in expected {
            let actual = ColumnEncoding::of_node_type(node_type)
                .unwrap_or_else(|error| panic!("{node_type:?} is supported: {error}"));
            assert_eq!(actual, encoding, "encoding of {node_type:?}");
        }
    }

    #[test]
    fn unsupported_node_types_are_rejected() {
        for node_type in [NodeType::DeprecatedDateString, NodeType::StructuredArray] {
            let result = ColumnEncoding::of_node_type(node_type);
            assert!(
                matches!(
                    result,
                    Err(LayoutError::UnsupportedNodeType {
                        node: None,
                        node_type: rejected,
                    }) if rejected == node_type
                ),
                "{node_type:?} maps to {result:?}"
            );
        }
    }

    #[test]
    fn only_delta_and_timestamp_columns_are_delta_encoded() {
        let delta_encoded = [ColumnEncoding::DeltaInt64, ColumnEncoding::Timestamp];
        for encoding in [
            ColumnEncoding::Int64,
            ColumnEncoding::DeltaInt64,
            ColumnEncoding::Float64,
            ColumnEncoding::FormattedFloat64,
            ColumnEncoding::DictionaryFloat,
            ColumnEncoding::Boolean,
            ColumnEncoding::ClpString,
            ColumnEncoding::VariableString,
            ColumnEncoding::UnstructuredArray,
            ColumnEncoding::Timestamp,
        ] {
            assert_eq!(
                encoding.is_delta_encoded(),
                delta_encoded.contains(&encoding),
                "{encoding:?}"
            );
        }
    }
}
