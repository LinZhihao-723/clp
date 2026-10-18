//! The public facade of the Rust clp-s archive reader: open single-file archives (local, in
//! memory or on S3), scan them with KQL queries through a pull cursor, or push the results of a
//! multi-archive session into a sink.
//!
//! This is the only crate hosts should depend on; everything else is internal.

pub use clp_s_exec::Archive;
pub use clp_s_exec::ArchiveId;
pub use clp_s_exec::ArchiveInfo;
pub use clp_s_exec::ArchiveLocation;
pub use clp_s_exec::ArchiveSummary;
pub use clp_s_exec::BatchRows;
pub use clp_s_exec::ExecutionConfig;
pub use clp_s_exec::Executor;
pub use clp_s_exec::LatestFirstKey;
pub use clp_s_exec::MergeOrder;
pub use clp_s_exec::OutputRequirements;
pub use clp_s_exec::OwnedBatch;
pub use clp_s_exec::PartialSink;
pub use clp_s_exec::PartitionInfo;
pub use clp_s_exec::PartitionKind;
pub use clp_s_exec::PartitionSummary;
pub use clp_s_exec::ReadError;
pub use clp_s_exec::ReaderContext;
pub use clp_s_exec::ReaderContextBuilder;
pub use clp_s_exec::RecordBatch;
pub use clp_s_exec::ResultOrder;
pub use clp_s_exec::Scan;
pub use clp_s_exec::ScanRequest;
pub use clp_s_exec::ScanRequestBuilder;
pub use clp_s_exec::ScanStats;
pub use clp_s_exec::SearchSession;
pub use clp_s_exec::SearchSessionBuilder;
pub use clp_s_exec::SessionReport;
pub use clp_s_exec::Sink;
pub use clp_s_exec::SinkControl;
pub use clp_s_exec::SinkError;
pub use clp_s_exec::TimestampRangeMs;
pub use clp_s_format::FormatVersion;
pub use clp_s_format::SharedBytes;
pub use clp_s_format::TableInfo;
pub use clp_s_io::ByteSource;
pub use clp_s_io::ReadContext;
pub use clp_s_io::SourceCapabilities;
pub use clp_s_json::JsonOptions;
pub use clp_s_json::JsonRecordBuffer;
pub use clp_s_query::ColumnPattern;
pub use clp_s_query::LogicalExpr;
pub use clp_s_query::Query;
pub use clp_s_query::QueryError;
pub use clp_s_query::QueryOptions;
pub use clp_s_query::TimeBounds;
pub use clp_s_resources::CancellationToken;
pub use clp_s_resources::ErrorKind;
pub use clp_s_resources::MemoryBudget;
pub use clp_s_resources::MemoryReservation;
pub use clp_s_resources::ResourceError;
pub use clp_s_search::PruneReason;
pub use clp_s_search::RecordProjection;
pub use clp_s_search::TerminationStage;
pub use clp_s_table::RecordId;

/// S3 sources (feature `s3`): an archive on S3 is opened as
/// `ArchiveLocation::Source(Arc::new(S3Source::new(..)?))`.
#[cfg(feature = "s3")]
pub mod s3 {
    pub use clp_s_object_store::ObjectStoreError;
    pub use clp_s_object_store::S3Client;
    pub use clp_s_object_store::S3Location;
    pub use clp_s_object_store::S3SdkConfig;
    pub use clp_s_object_store::S3Source;
    pub use clp_s_object_store::S3SourceConfig;
    pub use clp_s_object_store::S3Url;
}
