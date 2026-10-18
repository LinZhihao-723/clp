//! An in-process Spider search task: the counterpart of
//! `components/clp-tdl-package/src/task/query/search.rs` that searches one archive through the
//! reader library and writes the results into the results cache, instead of spawning `clp-s`.
//! Archives the library does not support are searched by spawning the C++ `clp-s`, exactly as the
//! package does today.

mod admission;
mod resources;
mod search;
mod subprocess;
mod tracking;

pub use resources::AdapterConfig;
pub use resources::AdapterResources;
pub use search::resolve_archive_location;
pub use search::search;
pub use subprocess::search_with_clp_s_subprocess;
