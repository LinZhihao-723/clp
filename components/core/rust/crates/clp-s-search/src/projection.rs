//! Projection requests.

use clp_s_query::ColumnPattern;

/// Which leaves of each matched record are rendered.
///
/// The C++ rules apply: literal paths only, through objects only, to leaf nodes only; every matched
/// record is emitted, as `{}` when it has none of the projected leaves.
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RecordProjection {
    #[default]
    Full,

    Columns(Vec<ColumnPattern>),
}
