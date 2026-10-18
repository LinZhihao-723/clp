//! Sans-IO parsing, validation and decoding of single-file clp-s archives (format 0.5.x).
//!
//! The crate turns archive bytes into validated, immutable structures (header, metadata packets,
//! schema tree and map, table directory, timestamp and range indexes, dictionaries) and decodes
//! packed streams into pooled buffers. It never performs I/O: drivers fetch the byte ranges it
//! asks for and push them back.

mod bytes_reader;
mod decode;
mod dictionary;
mod error;
mod header;
mod ids;
mod index;
mod limits;
mod metadata;
mod msgpack;
mod opener;
mod plan;
mod range;
mod range_index;
mod schema;
mod timestamp_index;
mod version;

pub use clp_s_resources::SharedBytes;
pub use decode::StreamDecodeSession;
pub use decode::StreamDecoder;
pub use decode::TableBytes;
pub use dictionary::ArchiveDictionaries;
pub use dictionary::DictionaryKind;
pub use dictionary::LogTypeDictionary;
pub use dictionary::Logtype;
pub use dictionary::LogtypePlaceholder;
pub use dictionary::VariableDictionary;
pub use error::CorruptionReason;
pub use error::FormatError;
pub use error::FormatLocation;
pub use error::UnsupportedFeature;
pub use header::ArchiveHeader;
pub use ids::LogTypeId;
pub use ids::NodeId;
pub use ids::SchemaId;
pub use ids::StreamId;
pub use ids::TableOrdinal;
pub use ids::TimestampPatternId;
pub use ids::VariableId;
pub use index::ArchiveIndex;
pub use limits::OpenOptions;
pub use limits::ReaderLimits;
pub use metadata::ArchiveMetadata;
pub use metadata::SectionId;
pub use metadata::SectionTable;
pub use metadata::UnknownPacket;
pub use opener::ArchiveOpener;
pub use opener::OpenStep;
pub use plan::DictionaryNeeds;
pub use plan::ReadPlan;
pub use plan::StreamRead;
pub use plan::TableSet;
pub use range::ByteRange;
pub use range_index::MetadataFields;
pub use range_index::MetadataValue;
pub use range_index::RangeIndex;
pub use range_index::RangeIndexEntry;
pub use schema::NodeTableIndex;
pub use schema::NodeType;
pub use schema::PackedStreamInfo;
pub use schema::Schema;
pub use schema::SchemaMap;
pub use schema::SchemaNodeView;
pub use schema::SchemaTree;
pub use schema::TableDirectory;
pub use schema::TableInfo;
pub use timestamp_index::TimestampIndex;
pub use timestamp_index::TimestampIndexEntry;
pub use timestamp_index::TimestampRange;
pub use version::ClassifiedVersion;
pub use version::FormatEpoch;
pub use version::FormatVersion;

const _: () = {
    const fn assert_send_sync<SendSyncType: Send + Sync + 'static>() {}
    const fn assert_send<SendType: Send>() {}

    assert_send_sync::<ArchiveMetadata>();
    assert_send_sync::<ArchiveIndex>();
    assert_send_sync::<ArchiveDictionaries>();
    assert_send_sync::<VariableDictionary>();
    assert_send_sync::<LogTypeDictionary>();
    assert_send_sync::<TableBytes>();
    assert_send::<StreamDecoder>();
};
