//! Table-layer tests over the C++-written fixture corpus: every fixture table decoded and compared
//! with the C++ `clp-s x` oracles, window and demand behaviour, record ids and timestamps, and the
//! error paths on mutated archives and table bytes.

mod datasets;
mod fixtures;
mod json;
mod mutations;
mod scan;
mod support;
