//! Arrow export of search results: the record batches and the Arrow IPC stream of the result
//! schema (archive ID, timestamp, message), and the protocol-v3 network sink that streams them to a
//! search-result listener.

mod network;
mod schema;
mod writer;

pub use network::ArrowNetworkPartial;
pub use network::ArrowNetworkSink;
pub use schema::ARCHIVE_ID_COLUMN;
pub use schema::MESSAGE_COLUMN;
pub use schema::RESULT_SCHEMA;
pub use schema::TIMESTAMP_COLUMN;
pub use writer::ResultBatchLimits;
pub use writer::ResultStreamWriter;
