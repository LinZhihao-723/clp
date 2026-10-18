//! The `MongoDB` results-cache sink: inserts search results as the C++ `ResultsCacheOutputHandler`
//! documents, with session-wide top-k when the request is
//! [`clp_s_reader::ResultOrder::LatestFirst`].

mod error;
mod sink;

pub use error::ResultsCacheError;
pub use sink::MongoDatabase;
pub use sink::ResultsCacheConfig;
pub use sink::ResultsCachePartial;
pub use sink::ResultsCacheSink;
pub use sink::connect_database;
