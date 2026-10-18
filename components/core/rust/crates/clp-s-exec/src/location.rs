//! Archive locations.

use std::path::PathBuf;
use std::sync::Arc;

use clp_s_format::SharedBytes;
use clp_s_io::ByteSource;

/// Where an archive lives, as a user names it; resolved into a source at open time.
#[non_exhaustive]
#[derive(Clone)]
pub enum ArchiveLocation {
    /// A single-file archive on the local file system. A directory (a multi-file archive) is
    /// rejected at open with an `UnsupportedArchive` error.
    File(PathBuf),

    /// A memory-resident single-file archive.
    Memory(SharedBytes),

    /// A host-provided source, such as `clp_s_object_store::S3Source`.
    Source(Arc<dyn ByteSource>),
}

impl std::fmt::Debug for ArchiveLocation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(path) => formatter.debug_tuple("File").field(path).finish(),
            Self::Memory(bytes) => formatter.debug_tuple("Memory").field(&bytes.len()).finish(),
            Self::Source(_) => formatter.debug_tuple("Source").finish_non_exhaustive(),
        }
    }
}
