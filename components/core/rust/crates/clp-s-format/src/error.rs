//! Errors raised while parsing and decoding archive bytes.

use clp_s_resources::ErrorKind;
use clp_s_resources::ResourceError;

use crate::FormatVersion;
use crate::NodeId;
use crate::SchemaId;
use crate::SectionId;
use crate::StreamId;
use crate::TableOrdinal;
use crate::TimestampPatternId;

/// Errors from parsing and decoding archive bytes; payloads are boxed to keep results small.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    #[error("corrupt archive in {location}: {reason}")]
    Corrupt {
        location: FormatLocation,
        reason: Box<CorruptionReason>,
    },

    #[error("unsupported archive version {0}")]
    UnsupportedVersion(FormatVersion),

    #[error("unsupported archive feature in {location}: {feature}")]
    UnsupportedFeature {
        location: FormatLocation,
        feature: UnsupportedFeature,
    },

    #[error("{what} of {actual_size} exceeds the limit of {limit_size}")]
    LimitExceeded {
        what: &'static str,
        actual_size: u64,
        limit_size: u64,
    },

    #[error("failed to allocate a zstd decompression context")]
    ZstdContextAllocationFailed,

    #[error(transparent)]
    Resource(#[from] ResourceError),
}

impl FormatError {
    /// # Returns
    ///
    /// The category this error maps onto.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        match self {
            Self::Corrupt { .. } => ErrorKind::CorruptArchive,
            Self::UnsupportedVersion(_) | Self::UnsupportedFeature { .. } => {
                ErrorKind::UnsupportedArchive
            }
            Self::LimitExceeded { .. } | Self::ZstdContextAllocationFailed => {
                ErrorKind::ResourceExhausted
            }
            Self::Resource(error) => error.kind(),
        }
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// A [`FormatError::Corrupt`] for `reason` at `location`.
    pub(crate) fn corrupt(location: FormatLocation, reason: CorruptionReason) -> Self {
        Self::Corrupt {
            location,
            reason: Box::new(reason),
        }
    }

    /// Checks that `actual_size` is within `limit_size`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::LimitExceeded`] if `actual_size` exceeds `limit_size`.
    pub(crate) const fn check_limit(
        what: &'static str,
        actual_size: u64,
        limit_size: u64,
    ) -> Result<(), Self> {
        if actual_size > limit_size {
            return Err(Self::LimitExceeded {
                what,
                actual_size,
                limit_size,
            });
        }
        Ok(())
    }
}

/// Where in the archive a format error was found.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatLocation {
    Header,
    MetadataSection,
    MetadataPacket { packet_type: u8 },
    Section(SectionId),
    PackedStream(StreamId),
    Table(TableOrdinal),
}

impl std::fmt::Display for FormatLocation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Header => write!(formatter, "the header"),
            Self::MetadataSection => write!(formatter, "the metadata section"),
            Self::MetadataPacket { packet_type } => {
                write!(formatter, "metadata packet of type {packet_type}")
            }
            Self::Section(section) => write!(formatter, "section {}", section.name()),
            Self::PackedStream(stream) => write!(formatter, "packed stream {}", stream.get()),
            Self::Table(table) => write!(formatter, "table {}", table.get()),
        }
    }
}

/// Why archive bytes are corrupt; one variant per structural check, so tests can assert the exact
/// reason. Builders add variants as they port checks.
#[non_exhaustive]
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum CorruptionReason {
    #[error("bad magic number")]
    BadMagic,

    #[error("the archive is truncated: {needed_size} bytes needed, {available_size} available")]
    Truncated {
        needed_size: u64,
        available_size: u64,
    },

    #[error("the header disagrees with the file size")]
    HeaderSizeMismatch,

    #[error("malformed msgpack")]
    MalformedMsgpack,

    #[error("a packet or section is shorter than its declared size")]
    ShortPayload,

    #[error("a count of {count} does not fit in the {remaining_size} remaining bytes")]
    CountExceedsRemaining { count: u64, remaining_size: u64 },

    #[error("the file-info packet is missing or invalid")]
    InvalidSectionTable,

    #[error("node {node} has an invalid parent")]
    InvalidParent { node: u32 },

    #[error("node {node} duplicates the (parent, key, type) of an earlier node")]
    DuplicateNode { node: u32 },

    #[error("unknown node type {0}")]
    UnknownNodeType(u8),

    #[error("node {0:?} does not exist")]
    UnknownNode(NodeId),

    #[error("schema {0:?} does not exist")]
    UnknownSchema(SchemaId),

    #[error("table offsets do not tile their packed stream")]
    TableTiling,

    #[error("a zstd frame does not fill its range exactly")]
    FrameSizeMismatch,

    #[error("decoded size {actual_size} differs from the expected {expected_size}")]
    DecodedSizeMismatch {
        expected_size: u64,
        actual_size: u64,
    },

    #[error("zstd decompression failed: {0}")]
    Zstd(Box<str>),

    #[error("id {id} is out of range for a dictionary of {len} entries")]
    IdOutOfRange { id: u64, len: u64 },

    #[error("invalid timestamp-index entry")]
    InvalidTimestampEntry,

    #[error("invalid range-index entry")]
    InvalidRangeIndexEntry,

    #[error("metadata packet type {0} appears more than once")]
    DuplicatePacket(u8),

    #[error("the archive has no {} section", .0.name())]
    MissingSection(SectionId),

    #[error("schema {0:?} is defined more than once")]
    DuplicateSchema(SchemaId),

    #[error("schema {0:?} declares more ordered nodes than nodes")]
    InvalidSchema(SchemaId),

    #[error("schema {0:?} has more than one table")]
    DuplicateTable(SchemaId),

    #[error("packed stream {0} does not exist")]
    UnknownStream(u64),

    #[error("packed-stream offsets are out of order or outside the packed-stream section")]
    InvalidStreamDirectory,

    #[error("the table does not lie inside its decoded stream")]
    TableOutsideStream,

    #[error("timestamp pattern {0:?} does not exist")]
    UnknownTimestampPattern(TimestampPatternId),

    #[error("a string is not valid UTF-8")]
    InvalidUtf8,

    #[error("the opener has not reached the index checkpoint")]
    OpenerNotReady,
}

/// A valid construct this reader does not support.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UnsupportedFeature {
    #[error("compression type {0}")]
    CompressionType(u16),

    #[error("{0} segments")]
    NumSegments(u64),

    #[error("structured arrays")]
    StructuredArrays,

    #[error("separate column schemas")]
    SeparateColumnSchemas,

    #[error("timestamp encoding {0}")]
    TimestampEncoding(u64),

    #[error("a table of {0} rows")]
    TableRowCount(u64),

    #[error("node type {0} in a newer patch release")]
    NewerNodeType(u8),

    #[error("timestamp pattern {0} that cannot be compiled")]
    TimestampPattern(u64),
}
