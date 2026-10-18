//! The hand-written KQL parser (a port of the grammar in `search/kql/Kql.g4`); its entry point is
//! [`crate::LogicalExpr::parse_kql`].

mod lexer;
mod literal;
mod parser;
mod timestamp;

pub use literal::unescape_kql;
pub use parser::parse;
