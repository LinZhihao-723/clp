//! Golden tests over the C++-written fixture corpus: rendered records against the C++ `clp-s x`
//! and `clp-s s --projection` oracles, partial selections and chosen rows, template caching, the
//! error paths of rendering, and no panic on mutated tables.

mod datasets;
mod errors;
mod fixtures;
mod mutations;
mod projection;
mod rows;
mod support;
