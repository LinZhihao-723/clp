//! An opened single-file archive: its index plus lazily loaded dictionaries and stream reads.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use clp_s_format::ArchiveDictionaries;
use clp_s_format::ArchiveIndex;
use clp_s_format::ArchiveOpener;
use clp_s_format::ByteRange;
use clp_s_format::CorruptionReason;
use clp_s_format::DictionaryNeeds;
use clp_s_format::FormatError;
use clp_s_format::FormatLocation;
use clp_s_format::LogTypeDictionary;
use clp_s_format::OpenOptions;
use clp_s_format::OpenStep;
use clp_s_format::ReaderLimits;
use clp_s_format::SectionId;
use clp_s_format::SharedBytes;
use clp_s_format::StreamDecoder;
use clp_s_format::StreamRead;
use clp_s_format::VariableDictionary;
use clp_s_resources::CancellationToken;
use clp_s_resources::PooledBuffer;
use clp_s_resources::ResourceError;

use crate::ByteSource;
use crate::ReadContext;
use crate::ResolvedSource;
use crate::StorageError;

/// Knobs for one stream read.
#[derive(Clone, Debug)]
pub struct StreamReadOptions {
    /// Compressed chunk size for local files (default 1 MiB).
    pub chunk_size: u64,

    /// Polled between chunks and passed to every read.
    pub cancellation_token: CancellationToken,
}

impl Default for StreamReadOptions {
    fn default() -> Self {
        Self {
            chunk_size: 1 << 20,
            cancellation_token: CancellationToken::new(),
        }
    }
}

/// An opened single-file archive: the validated index, plus dictionaries loaded on demand.
///
/// Opening reads the header, the metadata section and the schema layer (usually one 64 KiB read).
/// Nothing needs to be closed; dropping the storage releases everything.
#[derive(Debug)]
pub struct ArchiveStorage {
    source: ResolvedSource,
    limits: ReaderLimits,
    index: Arc<ArchiveIndex>,
    dictionaries: Arc<ArchiveDictionaries>,
    num_bytes_read: AtomicU64,
}

impl ArchiveStorage {
    /// Factory function.
    ///
    /// Drives [`clp_s_format::ArchiveOpener`] over `source` to
    /// [`clp_s_format::OpenStep::IndexReady`].
    ///
    /// # Returns
    ///
    /// The opened archive on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`StorageError::Io`] if a read fails.
    /// * [`StorageError::Cancelled`] if `cancellation_token` is cancelled.
    /// * [`StorageError::Resource`] if a read buffer cannot be allocated.
    /// * Forwards [`clp_s_format::ArchiveOpener::advance`]'s return values on failure.
    /// * Forwards [`clp_s_format::ArchiveOpener::push_range`]'s return values on failure.
    /// * Forwards [`clp_s_format::ArchiveOpener::into_index`]'s return values on failure.
    pub fn open(
        source: ResolvedSource,
        options: &OpenOptions,
        cancellation_token: &CancellationToken,
    ) -> Result<Self, StorageError> {
        let file_size = source.size().map_err(|source| StorageError::Io {
            range: ByteRange { begin: 0, end: 0 },
            source,
        })?;
        let num_bytes_read = AtomicU64::new(0);
        let mut opener = ArchiveOpener::new(*options, file_size);
        loop {
            match opener.advance()? {
                OpenStep::NeedsData(ranges) => {
                    for range in ranges {
                        let bytes = fetch(&source, range, cancellation_token, &num_bytes_read)?;
                        opener.push_range(range, bytes)?;
                    }
                }
                OpenStep::MetadataReady => {}
                OpenStep::IndexReady => break,
            }
        }
        let index = Arc::new(opener.into_index()?);
        tracing::debug!(
            file_size,
            num_tables = index.tables().tables().len(),
            num_bytes_read = num_bytes_read.load(Ordering::Relaxed),
            "Opened archive."
        );
        Ok(Self {
            source,
            limits: options.limits,
            index,
            dictionaries: Arc::new(ArchiveDictionaries::default()),
            num_bytes_read,
        })
    }

    /// # Returns
    ///
    /// The archive index.
    #[must_use]
    pub const fn index(&self) -> &Arc<ArchiveIndex> {
        &self.index
    }

    /// # Returns
    ///
    /// The archive's dictionary cells, filled by [`Self::load_dictionaries`].
    #[must_use]
    pub const fn dictionaries(&self) -> &Arc<ArchiveDictionaries> {
        &self.dictionaries
    }

    /// Loads every dictionary in `needs` that is not loaded yet; a failed load leaves its cell
    /// empty so the next call retries.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`StorageError::Io`] if a read fails.
    /// * [`StorageError::Cancelled`] if `cancellation_token` is cancelled.
    /// * [`StorageError::Resource`] if a read buffer cannot be allocated.
    /// * Forwards [`clp_s_format::VariableDictionary::load`]'s return values on failure.
    /// * Forwards [`clp_s_format::LogTypeDictionary::load`]'s return values on failure.
    /// * Forwards [`clp_s_format::LogTypeDictionary::load_array`]'s return values on failure.
    pub fn load_dictionaries(
        &self,
        needs: DictionaryNeeds,
        cancellation_token: &CancellationToken,
    ) -> Result<(), StorageError> {
        if needs.is_variable_needed {
            self.load_dictionary(
                &self.dictionaries.variable,
                SectionId::VariableDictionary,
                cancellation_token,
                VariableDictionary::load,
            )?;
        }
        if needs.is_logtype_needed {
            self.load_dictionary(
                &self.dictionaries.logtype,
                SectionId::LogTypeDictionary,
                cancellation_token,
                LogTypeDictionary::load,
            )?;
        }
        if needs.is_array_needed {
            self.load_dictionary(
                &self.dictionaries.array,
                SectionId::ArrayDictionary,
                cancellation_token,
                LogTypeDictionary::load_array,
            )?;
        }
        Ok(())
    }

    /// Fetches and decodes one planned stream into `buffer`: single-shot when the source is
    /// resident or remote (one read of the whole compressed frame), stable-output streaming in
    /// `options.chunk_size` chunks for local files.
    ///
    /// # Returns
    ///
    /// The decoded prefix of the stream on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`StorageError::Io`] if a read fails.
    /// * [`StorageError::Cancelled`] if `options.cancellation_token` is cancelled.
    /// * [`StorageError::Format`] if the stream exceeds [`ReaderLimits::max_packed_stream_size`].
    /// * [`StorageError::Resource`] if a read buffer cannot be allocated.
    /// * Forwards [`StreamDecoder::decode_resident`]'s return values on failure.
    /// * Forwards [`StreamDecoder::begin`]'s return values on failure.
    /// * Forwards [`clp_s_format::StreamDecodeSession::push_chunk`]'s return values on failure.
    /// * Forwards [`clp_s_format::StreamDecodeSession::finish`]'s return values on failure.
    pub fn read_stream(
        &self,
        read: &StreamRead,
        decoder: &mut StreamDecoder,
        buffer: PooledBuffer,
        options: &StreamReadOptions,
    ) -> Result<SharedBytes, StorageError> {
        let token = &options.cancellation_token;
        if read.stream.uncompressed_size > self.limits.max_packed_stream_size {
            return Err(FormatError::LimitExceeded {
                what: "packed stream size",
                actual_size: read.stream.uncompressed_size,
                limit_size: self.limits.max_packed_stream_size,
            }
            .into());
        }
        token.check().map_err(|_| StorageError::Cancelled)?;

        let range = read.stream.compressed_range;
        let capabilities = self.source.capabilities();
        if capabilities.is_resident || capabilities.is_remote {
            let frame = fetch(&self.source, range, token, &self.num_bytes_read)?;
            return Ok(decoder.decode_resident(&frame, read, buffer)?);
        }

        let mut session = decoder.begin(read, buffer)?;
        let chunk_size = options.chunk_size.clamp(1, range.len().max(1));
        let mut chunk = allocate(chunk_size)?;
        let mut offset = range.begin;
        while offset < range.end {
            token.check().map_err(|_| StorageError::Cancelled)?;
            let chunk_range = ByteRange {
                begin: offset,
                end: range.end.min(offset + chunk_size),
            };
            let chunk = &mut chunk[..usize::try_from(chunk_range.len()).unwrap_or(usize::MAX)];
            read_into(
                &self.source,
                chunk_range,
                chunk,
                token,
                &self.num_bytes_read,
            )?;
            if session.push_chunk(chunk)? {
                break;
            }
            offset = chunk_range.end;
        }
        Ok(session.finish()?)
    }

    /// # Returns
    ///
    /// The number of bytes read from the source so far.
    #[must_use]
    pub fn num_bytes_read(&self) -> u64 {
        self.num_bytes_read.load(Ordering::Relaxed)
    }

    /// Loads one dictionary into `cell` unless it is loaded already.
    ///
    /// # Type Parameters
    ///
    /// * `DictionaryType` - The dictionary type.
    /// * `LoaderType` - The function that decodes the section.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`StorageError::Format`] if the archive has no such section.
    /// * Forwards [`fetch`]'s return values on failure.
    /// * Forwards `load`'s return values on failure.
    fn load_dictionary<
        DictionaryType,
        LoaderType: FnOnce(&[u8], &ReaderLimits) -> Result<DictionaryType, FormatError>,
    >(
        &self,
        cell: &OnceLock<Arc<DictionaryType>>,
        section: SectionId,
        cancellation_token: &CancellationToken,
        load: LoaderType,
    ) -> Result<(), StorageError> {
        if cell.get().is_some() {
            return Ok(());
        }
        let range = self
            .index
            .metadata()
            .sections()
            .range(section)
            .ok_or_else(|| FormatError::Corrupt {
                location: FormatLocation::Section(section),
                reason: Box::new(CorruptionReason::MissingSection(section)),
            })?;
        let bytes = fetch(
            &self.source,
            range,
            cancellation_token,
            &self.num_bytes_read,
        )?;
        let dictionary = Arc::new(load(&bytes, &self.limits)?);
        tracing::debug!(
            section = section.name(),
            size = bytes.len(),
            "Loaded dictionary."
        );
        cell.get_or_init(|| dictionary);
        Ok(())
    }
}

/// Fetches `range` from `source`: a zero-copy slice if the source is resident, else a read into a
/// new buffer.
///
/// # Returns
///
/// The bytes on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`StorageError::Cancelled`] if `cancellation_token` is cancelled.
/// * Forwards [`allocate`]'s return values on failure.
/// * Forwards [`read_into`]'s return values on failure.
fn fetch(
    source: &ResolvedSource,
    range: ByteRange,
    cancellation_token: &CancellationToken,
    num_bytes_read: &AtomicU64,
) -> Result<SharedBytes, StorageError> {
    cancellation_token
        .check()
        .map_err(|_| StorageError::Cancelled)?;
    if let Some(bytes) = source.slice(range) {
        num_bytes_read.fetch_add(range.len(), Ordering::Relaxed);
        return Ok(bytes);
    }
    let mut buffer = allocate(range.len())?;
    read_into(
        source,
        range,
        &mut buffer,
        cancellation_token,
        num_bytes_read,
    )?;
    Ok(SharedBytes::from(buffer))
}

/// Reads `range` from `source` into `buffer` (exactly `range.len()` bytes).
///
/// # Errors
///
/// Returns an error if:
///
/// * [`StorageError::Io`] if the read fails.
fn read_into(
    source: &ResolvedSource,
    range: ByteRange,
    buffer: &mut [u8],
    cancellation_token: &CancellationToken,
    num_bytes_read: &AtomicU64,
) -> Result<(), StorageError> {
    let context = ReadContext { cancellation_token };
    source
        .read_exact_at(range.begin, buffer, &context)
        .map_err(|source| StorageError::Io { range, source })?;
    num_bytes_read.fetch_add(buffer.len() as u64, Ordering::Relaxed);
    Ok(())
}

/// # Returns
///
/// A zero-filled buffer of `size` bytes on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`StorageError::Resource`] if the buffer cannot be allocated.
fn allocate(size: u64) -> Result<Vec<u8>, StorageError> {
    let allocation_error = || StorageError::Resource(ResourceError::AllocationFailed { size });
    let size = usize::try_from(size).map_err(|_| allocation_error())?;
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(size)
        .map_err(|_| allocation_error())?;
    buffer.resize(size, 0);
    Ok(buffer)
}
