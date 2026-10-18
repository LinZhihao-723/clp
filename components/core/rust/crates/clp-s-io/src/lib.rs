//! Synchronous drivers of the sans-IO format layer: positional byte sources (local files, memory,
//! and host-provided sources such as S3) and the opened archive storage that loads the index,
//! dictionaries and packed streams through them.

mod error;
mod source;
mod storage;

pub use error::StorageError;
pub use source::ByteSource;
pub use source::FileSource;
pub use source::MemorySource;
pub use source::ReadContext;
pub use source::ResolvedSource;
pub use source::SourceCapabilities;
pub use storage::ArchiveStorage;
pub use storage::StreamReadOptions;

const _: () = {
    const fn assert_send_sync<SendSyncType: Send + Sync + 'static>() {}

    assert_send_sync::<ArchiveStorage>();
    assert_send_sync::<ResolvedSource>();
};
