//! Wire formats shared with the C++ `clp-s` consumers: the msgpack search-result tuple of the file
//! and network outputs (read back by the package and the api-server), the results-cache document
//! field names, and the timestamp unit conversion used on the wire.

mod error;
mod results_cache;
mod timestamp;
mod tuple;

pub use error::WireError;
pub use results_cache::ResultsCacheField;
pub use timestamp::epoch_ns_to_wire_ms;
pub use tuple::SearchResultTuple;
