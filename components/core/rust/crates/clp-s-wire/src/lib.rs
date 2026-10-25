//! Wire formats shared with the C++ `clp-s` consumers: the msgpack search-result tuple of the file
//! and network outputs (read back by the package and the api-server), the framing of the
//! protocol-v2 and v3 streaming outputs read and acknowledged by the search-result listener, the
//! results-cache document field names, and the timestamp unit conversion used on the wire.

mod encode;
mod error;
mod results_cache;
mod stream;
mod timestamp;
mod tuple;

pub use error::WireError;
pub use results_cache::ResultsCacheField;
pub use stream::StreamAck;
pub use stream::StreamHandshake;
pub use stream::StreamProtocolVersion;
pub use stream::StreamResult;
pub use stream::StreamTrailer;
pub use timestamp::epoch_ns_to_wire_ms;
pub use tuple::SearchResultTuple;
