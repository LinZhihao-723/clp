//! The archive-independent query layer: KQL parsing (the C++ grammar), the query IR, negation
//! normal form with Kleene three-valued constant folding, literal typing, and the reference
//! evaluator that is the executable form of the search semantics (design report §8).
//!
//! Nothing here depends on archive code, so the semantics can be checked against raw JSON records.

mod column;
mod compare;
mod error;
mod evaluate;
mod expr;
mod kql;
mod normalize;
mod query;
mod value;

pub use column::ColumnPattern;
pub use column::ColumnRef;
pub use column::Namespace;
pub use column::PathToken;
pub use error::QueryError;
pub use evaluate::JsonRecord;
pub use evaluate::RecordValueSource;
pub use evaluate::ScalarValue;
pub use expr::CompareOperator;
pub use expr::LogicalExpr;
pub use expr::Predicate;
pub use expr::PredicateTest;
pub use expr::TruthValue;
pub use query::Leaf;
pub use query::LeafId;
pub use query::NormalExpr;
pub use query::Query;
pub use query::QueryLimits;
pub use query::QueryOptions;
pub use query::TimeBounds;
pub use value::PatternPiece;
pub use value::QueryValue;
pub use value::SearchType;
pub use value::SearchTypeSet;
pub use value::TextPattern;

const _: () = {
    const fn assert_send_sync<SendSyncType: Send + Sync + 'static>() {}

    assert_send_sync::<Query>();
    assert_send_sync::<LogicalExpr>();
};
