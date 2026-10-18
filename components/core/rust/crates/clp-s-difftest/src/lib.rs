//! The differential harness of the Rust clp-s reader: runs the C++ `clp-s` and the Rust reader on
//! the same archives and queries, canonicalizes their outputs exactly as the corpus scripts do
//! (`tests/README.md`), compares them as multisets, and accepts a mismatch only when the
//! divergence registry explains it and the reference evaluator agrees with the Rust result.

pub mod canonical;
pub mod compare;
pub mod corpus;
pub mod cpp;
pub mod harness;
pub mod multiset;
pub mod perf;
pub mod probe;
pub mod reader;
pub mod reference;
pub mod registry;
pub mod rust;
pub mod self_test;
