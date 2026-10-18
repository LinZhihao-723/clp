//! Positional byte sources.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;

use clp_s_format::ByteRange;
use clp_s_format::SharedBytes;
use clp_s_resources::CancellationToken;

use crate::StorageError;

/// Positional, thread-safe, cancellable read access to one archive file.
///
/// This is an open set (host sources, the S3 adapter), so it is used as a trait object behind
/// [`ResolvedSource::Dyn`].
pub trait ByteSource: Send + Sync {
    /// Returns what this source can do cheaply; the read policy is derived from it.
    ///
    /// # Returns
    ///
    /// The capabilities of the source.
    fn capabilities(&self) -> SourceCapabilities;

    /// Returns the size of the file.
    ///
    /// # Returns
    ///
    /// The size in bytes on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`std::io::Error`] if the size cannot be determined.
    fn size(&self) -> Result<u64, std::io::Error>;

    /// Fills `buffer` completely with the bytes starting at `offset`.
    ///
    /// # Parameters
    ///
    /// * `offset` - Absolute offset within the file.
    /// * `buffer` - The buffer to fill.
    /// * `context` - The calling scan's cancellation token.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`std::io::Error`] if the read fails or is short; [`std::io::ErrorKind::Interrupted`]
    ///   means the read was cancelled through `context`.
    fn read_exact_at(
        &self,
        offset: u64,
        buffer: &mut [u8],
        context: &ReadContext<'_>,
    ) -> Result<(), std::io::Error>;

    /// Returns a zero-copy view of `range` if the bytes are memory-resident.
    ///
    /// # Parameters
    ///
    /// * `range` - Absolute range within the file.
    ///
    /// # Returns
    ///
    /// The bytes, or `None` if the source is not memory-resident.
    fn slice(&self, range: ByteRange) -> Option<SharedBytes> {
        let _ = range;
        None
    }
}

/// What a source can do cheaply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceCapabilities {
    /// Whether [`ByteSource::slice`] returns the bytes without a copy.
    pub is_resident: bool,

    /// Whether each read is a high-latency request (e.g. S3), so a compressed stream is fetched in
    /// one read instead of in chunks.
    pub is_remote: bool,
}

/// The context of one read.
#[derive(Clone, Copy, Debug)]
pub struct ReadContext<'context> {
    pub cancellation_token: &'context CancellationToken,
}

impl ReadContext<'_> {
    /// Checks whether the read is cancelled.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`std::io::ErrorKind::Interrupted`] if the cancellation token is cancelled.
    pub fn check(&self) -> Result<(), std::io::Error> {
        if self.cancellation_token.is_cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "the read was cancelled",
            ));
        }
        Ok(())
    }
}

/// A local file read with `pread`.
#[derive(Debug)]
pub struct FileSource {
    file: File,
    size: u64,
}

impl FileSource {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The source of the regular file at `path` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`std::io::ErrorKind::InvalidInput`] if `path` is not a regular file.
    /// * Forwards [`std::fs::File::open`]'s return values on failure.
    /// * Forwards [`std::fs::File::metadata`]'s return values on failure.
    pub fn open(path: &Path) -> Result<Self, std::io::Error> {
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        Ok(Self {
            file,
            size: metadata.len(),
        })
    }
}

impl ByteSource for FileSource {
    fn capabilities(&self) -> SourceCapabilities {
        SourceCapabilities {
            is_resident: false,
            is_remote: false,
        }
    }

    fn size(&self) -> Result<u64, std::io::Error> {
        Ok(self.size)
    }

    fn read_exact_at(
        &self,
        offset: u64,
        buffer: &mut [u8],
        context: &ReadContext<'_>,
    ) -> Result<(), std::io::Error> {
        context.check()?;
        self.file.read_exact_at(buffer, offset)
    }
}

/// Memory-resident archive bytes; reads are zero-copy slices.
#[derive(Clone, Debug)]
pub struct MemorySource {
    bytes: SharedBytes,
}

impl MemorySource {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A source over `bytes`.
    #[must_use]
    pub const fn new(bytes: SharedBytes) -> Self {
        Self { bytes }
    }

    /// # Returns
    ///
    /// The `size` bytes at `offset` as a `usize` range, or `None` if they are not inside the
    /// source.
    fn checked_range(&self, offset: u64, size: u64) -> Option<std::ops::Range<usize>> {
        let begin = usize::try_from(offset).ok()?;
        let end = begin.checked_add(usize::try_from(size).ok()?)?;
        (end <= self.bytes.len()).then_some(begin..end)
    }
}

impl ByteSource for MemorySource {
    fn capabilities(&self) -> SourceCapabilities {
        SourceCapabilities {
            is_resident: true,
            is_remote: false,
        }
    }

    fn size(&self) -> Result<u64, std::io::Error> {
        Ok(self.bytes.len() as u64)
    }

    fn read_exact_at(
        &self,
        offset: u64,
        buffer: &mut [u8],
        context: &ReadContext<'_>,
    ) -> Result<(), std::io::Error> {
        context.check()?;
        let range = self
            .checked_range(offset, buffer.len() as u64)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the read extends past the end of the source",
                )
            })?;
        buffer.copy_from_slice(&self.bytes[range]);
        Ok(())
    }

    fn slice(&self, range: ByteRange) -> Option<SharedBytes> {
        let range = self.checked_range(range.begin, range.end.checked_sub(range.begin)?)?;
        Some(self.bytes.slice(range))
    }
}

/// The concrete source of one archive file: built-ins by enum, everything else through `Dyn`.
pub enum ResolvedSource {
    File(FileSource),
    Memory(MemorySource),
    Dyn(Arc<dyn ByteSource>),
}

impl ResolvedSource {
    /// Factory function.
    ///
    /// Resolves a local path: a regular file is a single-file archive, and a directory is a
    /// multi-file archive, which this reader does not support.
    ///
    /// # Returns
    ///
    /// The file source on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`StorageError::MultiFileArchive`] if `path` is a directory.
    /// * [`StorageError::Open`] if `path` cannot be inspected or opened.
    pub fn from_path(path: &Path) -> Result<Self, StorageError> {
        let open_error = |source| StorageError::Open {
            path: path.to_path_buf(),
            source,
        };
        if std::fs::metadata(path).map_err(open_error)?.is_dir() {
            return Err(StorageError::MultiFileArchive {
                path: path.to_path_buf(),
            });
        }
        Ok(Self::File(FileSource::open(path).map_err(open_error)?))
    }
}

impl std::fmt::Debug for ResolvedSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(source) => formatter.debug_tuple("File").field(source).finish(),
            Self::Memory(source) => formatter.debug_tuple("Memory").field(source).finish(),
            Self::Dyn(_) => formatter.debug_tuple("Dyn").finish_non_exhaustive(),
        }
    }
}

impl ByteSource for ResolvedSource {
    fn capabilities(&self) -> SourceCapabilities {
        match self {
            Self::File(source) => source.capabilities(),
            Self::Memory(source) => source.capabilities(),
            Self::Dyn(source) => source.capabilities(),
        }
    }

    fn size(&self) -> Result<u64, std::io::Error> {
        match self {
            Self::File(source) => source.size(),
            Self::Memory(source) => source.size(),
            Self::Dyn(source) => source.size(),
        }
    }

    fn read_exact_at(
        &self,
        offset: u64,
        buffer: &mut [u8],
        context: &ReadContext<'_>,
    ) -> Result<(), std::io::Error> {
        match self {
            Self::File(source) => source.read_exact_at(offset, buffer, context),
            Self::Memory(source) => source.read_exact_at(offset, buffer, context),
            Self::Dyn(source) => source.read_exact_at(offset, buffer, context),
        }
    }

    fn slice(&self, range: ByteRange) -> Option<SharedBytes> {
        match self {
            Self::File(source) => source.slice(range),
            Self::Memory(source) => source.slice(range),
            Self::Dyn(source) => source.slice(range),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use clp_s_format::ByteRange;
    use clp_s_format::SharedBytes;
    use clp_s_resources::CancellationToken;
    use clp_s_resources::ErrorKind;

    use super::ByteSource;
    use super::FileSource;
    use super::MemorySource;
    use super::ReadContext;
    use super::ResolvedSource;
    use crate::StorageError;

    const CONTENT: &[u8] = b"0123456789abcdef";

    /// Reads `size` bytes at `offset` from `source`.
    ///
    /// # Returns
    ///
    /// The bytes on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`ByteSource::read_exact_at`]'s return values on failure.
    fn read(
        source: &dyn ByteSource,
        offset: u64,
        size: usize,
        token: &CancellationToken,
    ) -> Result<Vec<u8>, std::io::Error> {
        let mut buffer = vec![0; size];
        source.read_exact_at(
            offset,
            &mut buffer,
            &ReadContext {
                cancellation_token: token,
            },
        )?;
        Ok(buffer)
    }

    /// # Returns
    ///
    /// A temporary file holding [`CONTENT`] on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`tempfile::NamedTempFile::new`]'s return values on failure.
    /// * Forwards [`std::io::Write::write_all`]'s return values on failure.
    /// * Forwards [`std::io::Write::flush`]'s return values on failure.
    fn temporary_file() -> anyhow::Result<tempfile::NamedTempFile> {
        let mut file = tempfile::NamedTempFile::new()?;
        file.write_all(CONTENT)?;
        file.flush()?;
        Ok(file)
    }

    #[test]
    fn file_sources_read_positionally() -> anyhow::Result<()> {
        let file = temporary_file()?;
        let source = FileSource::open(file.path())?;
        let token = CancellationToken::new();
        assert_eq!(source.size()?, CONTENT.len() as u64);
        assert!(!source.capabilities().is_resident);
        assert!(!source.capabilities().is_remote);
        assert_eq!(read(&source, 10, 6, &token)?, b"abcdef");
        assert_eq!(read(&source, 0, 2, &token)?, b"01");
        assert_eq!(source.slice(ByteRange { begin: 0, end: 2 }), None);

        let error = read(&source, 12, 8, &token).expect_err("the read passes the end");
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        Ok(())
    }

    #[test]
    fn memory_sources_slice_without_copying() -> anyhow::Result<()> {
        let bytes = SharedBytes::from_static(CONTENT);
        let source = MemorySource::new(bytes.clone());
        let token = CancellationToken::new();
        assert_eq!(source.size()?, 16);
        assert!(source.capabilities().is_resident);
        assert_eq!(read(&source, 4, 3, &token)?, b"456");

        let slice = source
            .slice(ByteRange { begin: 2, end: 5 })
            .expect("the range is inside the source");
        assert_eq!(&slice[..], b"234");
        assert_eq!(slice.as_ptr(), bytes[2..].as_ptr());
        for range in [
            ByteRange { begin: 10, end: 17 },
            ByteRange { begin: 5, end: 2 },
            ByteRange {
                begin: u64::MAX,
                end: u64::MAX,
            },
        ] {
            assert_eq!(source.slice(range), None, "{range:?}");
        }
        let error = read(&source, u64::MAX, 1, &token).expect_err("the read passes the end");
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        Ok(())
    }

    #[test]
    fn cancelled_reads_are_interrupted() -> anyhow::Result<()> {
        let file = temporary_file()?;
        let token = CancellationToken::new();
        token.cancel();
        let sources: [Box<dyn ByteSource>; 2] = [
            Box::new(FileSource::open(file.path())?),
            Box::new(MemorySource::new(SharedBytes::from_static(CONTENT))),
        ];
        for source in &sources {
            let error = read(source.as_ref(), 0, 1, &token).expect_err("the token is cancelled");
            assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
            let storage_error = StorageError::Io {
                range: ByteRange { begin: 0, end: 1 },
                source: error,
            };
            assert_eq!(storage_error.kind(), ErrorKind::Cancelled);
        }
        Ok(())
    }

    #[test]
    fn resolving_paths_rejects_directories_and_missing_files() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let error = ResolvedSource::from_path(directory.path())
            .expect_err("a directory is a multi-file archive");
        assert!(
            matches!(error, StorageError::MultiFileArchive { .. }),
            "{error:?}"
        );
        assert_eq!(error.kind(), ErrorKind::UnsupportedArchive);

        let error = ResolvedSource::from_path(&directory.path().join("missing"))
            .expect_err("the file does not exist");
        assert!(matches!(error, StorageError::Open { .. }), "{error:?}");
        assert_eq!(error.kind(), ErrorKind::Io);

        let error = FileSource::open(directory.path()).expect_err("a directory is not a file");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);

        let file = temporary_file()?;
        let source = ResolvedSource::from_path(file.path())?;
        assert!(matches!(source, ResolvedSource::File(_)), "{source:?}");
        assert_eq!(source.size()?, CONTENT.len() as u64);
        Ok(())
    }
}
