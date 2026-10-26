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
use clp_s_format::StreamDecodeSession;
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
    /// * [`StorageError::Cancelled`] if `options.cancellation_token` is cancelled.
    /// * [`StorageError::Format`] if the stream exceeds [`ReaderLimits::max_packed_stream_size`].
    /// * Forwards [`fetch`]'s return values on failure.
    /// * Forwards [`StreamDecoder::decode_resident`]'s return values on failure.
    /// * Forwards [`Self::begin_stream`]'s return values on failure.
    /// * Forwards [`Self::decode_stream_to`]'s return values on failure.
    /// * Forwards [`StreamReader::finish`]'s return values on failure.
    pub fn read_stream(
        &self,
        read: &StreamRead,
        decoder: &mut StreamDecoder,
        buffer: PooledBuffer,
        options: &StreamReadOptions,
    ) -> Result<SharedBytes, StorageError> {
        let capabilities = self.source.capabilities();
        if capabilities.is_resident || capabilities.is_remote {
            self.check_stream_read(read, options)?;
            let frame = fetch(
                &self.source,
                read.stream.compressed_range,
                &options.cancellation_token,
                &self.num_bytes_read,
            )?;
            return Ok(decoder.decode_resident(&frame, read, buffer)?);
        }
        let mut reader = self.begin_stream(read, decoder, buffer, options)?;
        self.decode_stream_to(&mut reader, decoder, read.decoded_prefix_size, options)?;
        reader.finish()
    }

    /// Starts a stepwise decode of one planned stream into `buffer`, which
    /// [`Self::decode_stream_to`] advances. A resident or remote source is fetched whole up front;
    /// a local file is read in `options.chunk_size` chunks as decoding proceeds.
    ///
    /// # Returns
    ///
    /// The reader on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`StorageError::Cancelled`] if `options.cancellation_token` is cancelled.
    /// * [`StorageError::Format`] if the stream exceeds [`ReaderLimits::max_packed_stream_size`].
    /// * Forwards [`fetch`]'s return values on failure.
    /// * Forwards [`allocate`]'s return values on failure.
    /// * Forwards [`StreamDecoder::begin`]'s return values on failure.
    pub fn begin_stream(
        &self,
        read: &StreamRead,
        decoder: &mut StreamDecoder,
        buffer: PooledBuffer,
        options: &StreamReadOptions,
    ) -> Result<StreamReader, StorageError> {
        self.check_stream_read(read, options)?;
        let range = read.stream.compressed_range;
        let capabilities = self.source.capabilities();
        let input = if capabilities.is_resident || capabilities.is_remote {
            StreamInput::Frame {
                frame: fetch(
                    &self.source,
                    range,
                    &options.cancellation_token,
                    &self.num_bytes_read,
                )?,
                position: 0,
            }
        } else {
            StreamInput::Chunks {
                chunk: allocate(options.chunk_size.clamp(1, range.len().max(1)))?,
                pending: 0..0,
                next_offset: range.begin,
            }
        };
        Ok(StreamReader {
            read: StreamRead {
                stream: read.stream,
                decoded_prefix_size: read.decoded_prefix_size,
                tables: Vec::new(),
            },
            input,
            session: decoder.begin(read, buffer)?,
            num_decoded_bytes: 0,
        })
    }

    /// Advances `reader` until the first `target_size` bytes of its stream are decoded (for the
    /// planned prefix of a whole stream, until the frame ends).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`StorageError::Cancelled`] if `options.cancellation_token` is cancelled.
    /// * [`StorageError::Format`] if the frame ends, or stops making progress, before the target.
    /// * Forwards [`read_into`]'s return values on failure.
    /// * Forwards [`clp_s_format::StreamDecodeSession::decode_until`]'s return values on failure.
    ///
    /// # Panics
    ///
    /// Panics if `decoder` did not begin the reader's current session, if the reader's decoded
    /// bytes are shared (see [`StreamReader::try_reclaim`]), or if `target_size` exceeds the
    /// planned prefix.
    pub fn decode_stream_to(
        &self,
        reader: &mut StreamReader,
        decoder: &mut StreamDecoder,
        target_size: u64,
        options: &StreamReadOptions,
    ) -> Result<(), StorageError> {
        let token = &options.cancellation_token;
        let range = reader.read.stream.compressed_range;
        let decoded_size_before = reader.session.decoded().len();
        let StreamReader { input, session, .. } = reader;
        while !session.has_decoded(target_size) {
            token.check().map_err(|_| StorageError::Cancelled)?;
            let consumed = match input {
                StreamInput::Frame { frame, position } => {
                    if *position == frame.len() {
                        break;
                    }
                    let consumed =
                        session.decode_until(decoder, &frame[*position..], target_size)?;
                    *position += consumed;
                    consumed
                }
                StreamInput::Chunks {
                    chunk,
                    pending,
                    next_offset,
                } => {
                    if pending.start == pending.end {
                        if *next_offset == range.end {
                            break;
                        }
                        let chunk_range = ByteRange {
                            begin: *next_offset,
                            end: range.end.min(*next_offset + chunk.len() as u64),
                        };
                        let chunk_size = usize::try_from(chunk_range.len()).unwrap_or(usize::MAX);
                        read_into(
                            &self.source,
                            chunk_range,
                            &mut chunk[..chunk_size],
                            token,
                            &self.num_bytes_read,
                        )?;
                        *pending = 0..chunk_size;
                        *next_offset = chunk_range.end;
                    }
                    let consumed =
                        session.decode_until(decoder, &chunk[pending.clone()], target_size)?;
                    pending.start += consumed;
                    consumed
                }
            };
            if 0 == consumed && !session.has_decoded(target_size) {
                break;
            }
        }
        let decoded_size = session.decoded().len();
        reader.num_decoded_bytes += decoded_size.saturating_sub(decoded_size_before) as u64;
        if !reader.session.has_decoded(target_size) {
            return Err(FormatError::Corrupt {
                location: FormatLocation::PackedStream(reader.read.stream.id),
                reason: Box::new(CorruptionReason::DecodedSizeMismatch {
                    expected_size: target_size,
                    actual_size: decoded_size as u64,
                }),
            }
            .into());
        }
        Ok(())
    }

    /// # Returns
    ///
    /// The number of bytes read from the source so far.
    #[must_use]
    pub fn num_bytes_read(&self) -> u64 {
        self.num_bytes_read.load(Ordering::Relaxed)
    }

    /// Checks that `read` may be decoded under the reader limits and the cancellation token.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`StorageError::Format`] if the stream exceeds [`ReaderLimits::max_packed_stream_size`].
    /// * [`StorageError::Cancelled`] if `options.cancellation_token` is cancelled.
    fn check_stream_read(
        &self,
        read: &StreamRead,
        options: &StreamReadOptions,
    ) -> Result<(), StorageError> {
        if read.stream.uncompressed_size > self.limits.max_packed_stream_size {
            return Err(FormatError::LimitExceeded {
                what: "packed stream size",
                actual_size: read.stream.uncompressed_size,
                limit_size: self.limits.max_packed_stream_size,
            }
            .into());
        }
        options
            .cancellation_token
            .check()
            .map_err(|_| StorageError::Cancelled)
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

/// One planned stream being decoded in steps by [`ArchiveStorage::decode_stream_to`].
#[derive(Debug)]
pub struct StreamReader {
    /// The planned stream, without its tables.
    read: StreamRead,

    input: StreamInput,
    session: StreamDecodeSession,

    /// The number of bytes decoded so far, including any decoded again after a restart.
    num_decoded_bytes: u64,
}

impl StreamReader {
    /// # Returns
    ///
    /// The bytes of the stream decoded so far.
    #[must_use]
    pub fn decoded(&self) -> &[u8] {
        self.session.decoded()
    }

    /// # Returns
    ///
    /// The number of bytes decoded so far, including any decoded again after a restart.
    #[must_use]
    pub const fn num_decoded_bytes(&self) -> u64 {
        self.num_decoded_bytes
    }

    /// Shares the bytes decoded so far without copying.
    ///
    /// # Returns
    ///
    /// The decoded bytes.
    pub fn share(&mut self) -> SharedBytes {
        self.session.share()
    }

    /// Takes the decode buffer back once every byte shared from it is dropped, so decoding can
    /// continue in place.
    ///
    /// # Returns
    ///
    /// Whether decoding can continue: `false` while shared bytes are still alive.
    pub fn try_reclaim(&mut self) -> bool {
        self.session.try_reclaim()
    }

    /// Restarts the decode from the beginning of the frame into `buffer`, leaving the current
    /// buffer to whoever still shares it.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`StreamDecoder::begin`]'s return values on failure.
    pub fn restart(
        &mut self,
        decoder: &mut StreamDecoder,
        buffer: PooledBuffer,
    ) -> Result<(), StorageError> {
        self.session = decoder.begin(&self.read, buffer)?;
        match &mut self.input {
            StreamInput::Frame { position, .. } => *position = 0,
            StreamInput::Chunks {
                pending,
                next_offset,
                ..
            } => {
                *pending = 0..0;
                *next_offset = self.read.stream.compressed_range.begin;
            }
        }
        Ok(())
    }

    /// Finishes the decode.
    ///
    /// # Returns
    ///
    /// The decoded prefix on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`StreamDecodeSession::finish`]'s return values on failure.
    pub fn finish(self) -> Result<SharedBytes, StorageError> {
        Ok(self.session.finish()?)
    }
}

/// Where a [`StreamReader`] gets the compressed frame from.
#[derive(Debug)]
enum StreamInput {
    /// The whole frame, fetched from a resident or remote source.
    Frame { frame: SharedBytes, position: usize },

    /// A local file, read one chunk at a time.
    Chunks {
        chunk: Vec<u8>,

        /// The bytes of `chunk` not consumed yet.
        pending: std::ops::Range<usize>,

        /// The file offset of the next chunk.
        next_offset: u64,
    },
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
