//! The execution engine of the clp-s reader: reader contexts, opened archives, scan requests, the
//! serial per-archive pipeline (open, plan, read, decode, filter, render), the pull cursor, the
//! push session with its sink protocol, session-wide top-k, statistics and the public error type.
//!
//! The public types of the reader originate here and are re-exported by `clp-s-reader`.

mod archive;
mod batch;
mod context;
mod error;
mod ids;
mod location;
mod request;
mod scan;
mod session;
mod sink;
mod stats;
mod topk;

pub use archive::Archive;
pub use archive::ArchiveInfo;
pub use archive::TimestampRangeMs;
pub use batch::BatchRows;
pub use batch::OwnedBatch;
pub use batch::RecordBatch;
pub use context::ExecutionConfig;
pub use context::Executor;
pub use context::ReaderContext;
pub use context::ReaderContextBuilder;
pub use error::ReadError;
pub use ids::ArchiveId;
pub use location::ArchiveLocation;
pub use request::OutputRequirements;
pub use request::ResultOrder;
pub use request::ScanRequest;
pub use request::ScanRequestBuilder;
pub use scan::Scan;
pub use session::ArchiveSummary;
pub use session::SearchSession;
pub use session::SearchSessionBuilder;
pub use session::SessionReport;
pub use sink::MergeOrder;
pub use sink::PartialSink;
pub use sink::PartitionInfo;
pub use sink::PartitionKind;
pub use sink::PartitionSummary;
pub use sink::Sink;
pub use sink::SinkControl;
pub use sink::SinkError;
pub use stats::ScanStats;
pub use topk::LatestFirstKey;

const _: () = {
    const fn assert_send_sync<SendSyncType: Send + Sync + 'static>() {}
    const fn assert_send<SendType: Send + 'static>() {}

    assert_send_sync::<ReaderContext>();
    assert_send_sync::<Archive>();
    assert_send_sync::<ScanRequest>();
    assert_send_sync::<ReadError>();
    assert_send::<Scan>();
    assert_send::<SearchSession>();
    assert_send::<OwnedBatch>();
    assert!(std::mem::size_of::<ReadError>() <= 32);
};
