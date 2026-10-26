//! A listener that receives the search results `clp-s` search tasks stream over TCP, as msgpack
//! frames (protocol v2) or an Arrow IPC stream (protocol v3), and delivers each result of a query
//! job to its consumer exactly once, encoded in batches for the consumer's transport, even when
//! Spider reruns tasks.

mod arrow_stream;
mod channel;
mod connection;
mod cursor;
mod encoding;
mod error;
mod job_status;
mod listener;
mod protocol;
mod session;
#[cfg(feature = "test-utils")]
pub mod test_utils;

pub use arrow_stream::ARCHIVE_ID_COLUMN;
pub use arrow_stream::ArrowResultBatch;
pub use arrow_stream::MESSAGE_COLUMN;
pub use arrow_stream::RESULT_SCHEMA;
pub use arrow_stream::TIMESTAMP_COLUMN;
pub use encoding::EncodedBatch;
pub use encoding::ResultEncoder;
pub use encoding::ResultEncoding;
pub use encoding::SearchResult;
pub use encoding::SearchResultEncoder;
pub use encoding::SearchResultEncoding;
pub use error::Error;
pub use job_status::JobStatusSource;
pub use job_status::JobStatusTable;
pub use job_status::MariaDbJobStatusSource;
pub use job_status::MariaDbJobStatusTable;
pub use job_status::PolledJobStatusSource;
pub use listener::ListenerConfig;
pub use listener::ResultListener;
pub use session::OutcomeFuture;
pub use session::ResultStream;
pub use session::Session;
pub use session::SessionConfig;
pub use session::SessionOutcome;
pub use session::SessionStats;
