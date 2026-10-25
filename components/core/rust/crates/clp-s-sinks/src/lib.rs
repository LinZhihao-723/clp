//! Built-in sinks of the clp-s reader: JSON lines to any writer (stdout, the `original` file of
//! `extract`), msgpack search-result tuples to a file or a network peer (C++-compatible), the
//! protocol-v2 stream to a search-result listener (C++-compatible), and an in-memory collector for
//! tests.

mod collect;
mod json_lines;
mod msgpack;
mod network;

pub use collect::CollectPartial;
pub use collect::CollectSink;
pub use collect::CollectedRecord;
pub use json_lines::JsonLinesPartial;
pub use json_lines::JsonLinesSink;
pub use msgpack::MsgpackResultPartial;
pub use msgpack::MsgpackResultSink;
pub use network::NetworkDestination;
pub use network::NetworkPartial;
pub use network::NetworkSink;
