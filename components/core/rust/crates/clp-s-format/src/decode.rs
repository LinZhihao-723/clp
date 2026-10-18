//! Decoding of packed streams and sections into pooled buffers.

use clp_s_resources::PooledBuffer;
use clp_s_resources::ResourceError;
use zstd_safe::DCtx;
use zstd_safe::DParameter;
use zstd_safe::InBuffer;
use zstd_safe::OutBuffer;
use zstd_safe::ResetDirective;
use zstd_safe::WriteBuf;

use crate::CorruptionReason;
use crate::FormatError;
use crate::FormatLocation;
use crate::ReaderLimits;
use crate::SharedBytes;
use crate::StreamRead;
use crate::TableInfo;

/// Per-worker zstd state, reused across streams and archives.
///
/// Packed streams are sized exactly from the table metadata; sections, whose frames store no
/// content size, are sized by `ZSTD_decompressBound` under [`ReaderLimits`].
pub struct StreamDecoder {
    context: DCtx<'static>,
}

impl StreamDecoder {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A new decoder on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::ZstdContextAllocationFailed`] if zstd cannot allocate its context.
    pub fn new() -> Result<Self, FormatError> {
        let context = DCtx::try_create().ok_or(FormatError::ZstdContextAllocationFailed)?;
        Ok(Self { context })
    }

    /// Decodes a memory-resident frame single-shot into `buffer`, up to
    /// `read.decoded_prefix_size`.
    ///
    /// # Returns
    ///
    /// The decoded prefix on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if the frame is malformed, does not fill its range exactly, or
    ///   decodes to a size other than the planned one.
    /// * [`FormatError::LimitExceeded`] if the planned size exceeds the capacity of `buffer`.
    pub fn decode_resident(
        &mut self,
        frame: &[u8],
        read: &StreamRead,
        mut buffer: PooledBuffer,
    ) -> Result<SharedBytes, FormatError> {
        let plan = DecodePlan::new(read, &mut buffer)?;
        let location = plan.location;
        let corrupt = |reason| FormatError::corrupt(location, reason);
        if 0 == plan.prefix_size {
            return Ok(buffer.freeze().into_shared_bytes());
        }
        let compressed_size = zstd_safe::find_frame_compressed_size(frame)
            .map_err(|code| corrupt(zstd_reason(code)))?;
        if compressed_size != frame.len() {
            return Err(corrupt(CorruptionReason::FrameSizeMismatch));
        }

        let mut output = PrefixBuffer::new(buffer.as_mut_vec(), plan.prefix_size);
        let decoded_size = if plan.is_full_stream {
            self.context
                .decompress(&mut output, frame)
                .map_err(|code| corrupt(zstd_reason(code)))?
        } else {
            self.reset(false)
                .map_err(|code| corrupt(zstd_reason(code)))?;
            let mut out_buffer = OutBuffer::around(&mut output);
            let mut in_buffer = InBuffer::around(frame);
            loop {
                let positions = (in_buffer.pos(), out_buffer.pos());
                let hint = self
                    .context
                    .decompress_stream(&mut out_buffer, &mut in_buffer)
                    .map_err(|code| corrupt(zstd_reason(code)))?;
                let is_stalled = positions == (in_buffer.pos(), out_buffer.pos());
                if out_buffer.pos() == plan.prefix_size || 0 == hint || is_stalled {
                    break;
                }
            }
            out_buffer.pos()
        };
        if decoded_size != plan.prefix_size {
            return Err(corrupt(CorruptionReason::DecodedSizeMismatch {
                expected_size: plan.prefix_size as u64,
                actual_size: decoded_size as u64,
            }));
        }
        Ok(buffer.freeze().into_shared_bytes())
    }

    /// Starts a stable-output decode of `read` fed with chunks of the compressed frame.
    ///
    /// # Returns
    ///
    /// The decode session on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::LimitExceeded`] if the planned size exceeds the capacity of `buffer`.
    /// * [`FormatError::Corrupt`] if the zstd context cannot be reset.
    pub fn begin(
        &mut self,
        read: &StreamRead,
        mut buffer: PooledBuffer,
    ) -> Result<StreamDecodeSession<'_>, FormatError> {
        let plan = DecodePlan::new(read, &mut buffer)?;
        // Stable output requires the buffer to hold the whole frame, so only full decodes use it.
        self.reset(plan.is_full_stream)
            .map_err(|code| FormatError::corrupt(plan.location, zstd_reason(code)))?;
        Ok(StreamDecodeSession {
            decoder: self,
            buffer,
            plan,
            output_position: 0,
            consumed_size: 0,
            is_frame_finished: false,
        })
    }

    /// Decodes a section frame whose decoded size is not stored.
    ///
    /// # Returns
    ///
    /// The decoded section on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if the frame is malformed or does not fill `frame` exactly.
    /// * [`FormatError::LimitExceeded`] if the decompression bound exceeds
    ///   [`ReaderLimits::max_section_decoded_size`].
    /// * [`FormatError::Resource`] if the decoded buffer cannot be allocated.
    pub fn decode_section(
        &mut self,
        frame: &[u8],
        location: FormatLocation,
        limits: &ReaderLimits,
    ) -> Result<SharedBytes, FormatError> {
        let corrupt = |reason| FormatError::corrupt(location, reason);
        let compressed_size = zstd_safe::find_frame_compressed_size(frame)
            .map_err(|code| corrupt(zstd_reason(code)))?;
        if compressed_size != frame.len() {
            return Err(corrupt(CorruptionReason::FrameSizeMismatch));
        }
        let bound =
            zstd_safe::decompress_bound(frame).map_err(|code| corrupt(zstd_reason(code)))?;
        FormatError::check_limit(
            "decoded section size",
            bound,
            limits.max_section_decoded_size,
        )?;
        let allocation_error =
            || FormatError::Resource(ResourceError::AllocationFailed { size: bound });
        let capacity = usize::try_from(bound).map_err(|_| allocation_error())?;
        let mut decoded: Vec<u8> = Vec::new();
        decoded
            .try_reserve_exact(capacity)
            .map_err(|_| allocation_error())?;
        self.context
            .decompress(&mut PrefixBuffer::new(&mut decoded, capacity), frame)
            .map_err(|code| corrupt(zstd_reason(code)))?;
        Ok(SharedBytes::from(decoded))
    }

    /// Resets the context for a new frame.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`DCtx::reset`]'s return values on failure.
    /// * Forwards [`DCtx::set_parameter`]'s return values on failure.
    fn reset(&mut self, is_output_stable: bool) -> Result<(), zstd_safe::ErrorCode> {
        self.context.reset(ResetDirective::SessionOnly)?;
        self.context
            .set_parameter(DParameter::StableOutBuffer(is_output_stable))?;
        Ok(())
    }
}

impl std::fmt::Debug for StreamDecoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StreamDecoder")
            .finish_non_exhaustive()
    }
}

/// An in-progress stable-output decode into one pooled buffer.
#[derive(Debug)]
pub struct StreamDecodeSession<'decoder> {
    decoder: &'decoder mut StreamDecoder,
    buffer: PooledBuffer,
    plan: DecodePlan,
    output_position: usize,
    consumed_size: u64,
    is_frame_finished: bool,
}

impl StreamDecodeSession<'_> {
    /// Feeds the next chunk of the compressed frame.
    ///
    /// # Returns
    ///
    /// Whether the planned prefix has been fully decoded, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if the frame is malformed or decodes past the stream size.
    pub fn push_chunk(&mut self, chunk: &[u8]) -> Result<bool, FormatError> {
        if self.is_done() {
            return Ok(true);
        }
        let location = self.plan.location;
        let corrupt = |reason| FormatError::corrupt(location, reason);
        self.consumed_size += chunk.len() as u64;
        if self.consumed_size > self.plan.compressed_size || self.is_frame_finished {
            return Err(corrupt(CorruptionReason::FrameSizeMismatch));
        }

        let mut output = PrefixBuffer::new(self.buffer.as_mut_vec(), self.plan.prefix_size);
        let mut out_buffer = OutBuffer::around_pos(&mut output, self.output_position);
        let mut in_buffer = InBuffer::around(chunk);
        while in_buffer.pos() < chunk.len() {
            let is_output_full = out_buffer.pos() == self.plan.prefix_size;
            if is_output_full && !self.plan.is_full_stream {
                break;
            }
            let positions = (in_buffer.pos(), out_buffer.pos());
            let hint = self
                .decoder
                .context
                .decompress_stream(&mut out_buffer, &mut in_buffer)
                .map_err(|code| corrupt(zstd_reason(code)))?;
            if positions == (in_buffer.pos(), out_buffer.pos()) && 0 != hint {
                break;
            }
            if 0 == hint {
                self.is_frame_finished = true;
                if in_buffer.pos() < chunk.len() {
                    return Err(corrupt(CorruptionReason::FrameSizeMismatch));
                }
                break;
            }
        }
        self.output_position = out_buffer.pos();

        if self.is_frame_finished && self.output_position < self.plan.prefix_size {
            return Err(corrupt(CorruptionReason::DecodedSizeMismatch {
                expected_size: self.plan.prefix_size as u64,
                actual_size: self.output_position as u64,
            }));
        }
        Ok(self.is_done())
    }

    /// Finishes the session.
    ///
    /// # Returns
    ///
    /// The decoded prefix on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if the planned prefix was not fully decoded.
    pub fn finish(self) -> Result<SharedBytes, FormatError> {
        if !self.is_done() {
            return Err(FormatError::corrupt(
                self.plan.location,
                CorruptionReason::DecodedSizeMismatch {
                    expected_size: self.plan.prefix_size as u64,
                    actual_size: self.output_position as u64,
                },
            ));
        }
        Ok(self.buffer.freeze().into_shared_bytes())
    }

    /// # Returns
    ///
    /// Whether the planned prefix is decoded, and, for a full-stream decode, the frame has ended.
    const fn is_done(&self) -> bool {
        self.output_position == self.plan.prefix_size
            && (self.is_frame_finished || !self.plan.is_full_stream || 0 == self.plan.prefix_size)
    }
}

/// One ERT's bytes inside a decoded stream: the unit handed to the table layer.
#[derive(Clone, Debug)]
pub struct TableBytes {
    bytes: SharedBytes,
    info: TableInfo,
}

impl TableBytes {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The bytes of table `info` inside `decoded_stream` (a decoded prefix of its stream) on
    /// success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if the table does not lie inside `decoded_stream`.
    pub fn new(decoded_stream: &SharedBytes, info: TableInfo) -> Result<Self, FormatError> {
        let range = usize::try_from(info.stream_offset)
            .ok()
            .zip(usize::try_from(info.uncompressed_size).ok())
            .and_then(|(begin, size)| Some(begin..begin.checked_add(size)?))
            .filter(|range| range.end <= decoded_stream.len())
            .ok_or_else(|| {
                FormatError::corrupt(
                    FormatLocation::Table(info.ordinal),
                    CorruptionReason::TableOutsideStream,
                )
            })?;
        Ok(Self {
            bytes: decoded_stream.slice(range),
            info,
        })
    }

    /// # Returns
    ///
    /// The ERT bytes: unaligned columns back to back, in schema order.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// # Returns
    ///
    /// The table these bytes belong to.
    #[must_use]
    pub const fn info(&self) -> &TableInfo {
        &self.info
    }

    /// # Returns
    ///
    /// The ERT bytes as shared bytes.
    #[must_use]
    pub const fn shared_bytes(&self) -> &SharedBytes {
        &self.bytes
    }
}

/// Decodes one section frame with a fresh zstd context.
///
/// # Returns
///
/// The decoded section on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`StreamDecoder::new`]'s return values on failure.
/// * Forwards [`StreamDecoder::decode_section`]'s return values on failure.
pub fn decode_section_frame(
    frame: &[u8],
    location: FormatLocation,
    limits: &ReaderLimits,
) -> Result<SharedBytes, FormatError> {
    StreamDecoder::new()?.decode_section(frame, location, limits)
}

/// The validated sizes of one stream decode.
#[derive(Clone, Copy, Debug)]
struct DecodePlan {
    location: FormatLocation,
    prefix_size: usize,
    compressed_size: u64,
    is_full_stream: bool,
}

impl DecodePlan {
    /// Factory function.
    ///
    /// Validates `read` and clears `buffer` for decoding.
    ///
    /// # Returns
    ///
    /// The plan on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if the planned prefix is larger than the stream.
    /// * [`FormatError::LimitExceeded`] if the planned prefix exceeds the capacity of `buffer`.
    fn new(read: &StreamRead, buffer: &mut PooledBuffer) -> Result<Self, FormatError> {
        let location = FormatLocation::PackedStream(read.stream.id);
        if read.decoded_prefix_size > read.stream.uncompressed_size {
            return Err(FormatError::corrupt(
                location,
                CorruptionReason::DecodedSizeMismatch {
                    expected_size: read.stream.uncompressed_size,
                    actual_size: read.decoded_prefix_size,
                },
            ));
        }
        let vec = buffer.as_mut_vec();
        vec.clear();
        let capacity = vec.capacity() as u64;
        FormatError::check_limit("planned stream prefix", read.decoded_prefix_size, capacity)?;
        Ok(Self {
            location,
            prefix_size: usize::try_from(read.decoded_prefix_size).unwrap_or(usize::MAX),
            compressed_size: read.stream.compressed_range.len(),
            is_full_stream: read.decoded_prefix_size == read.stream.uncompressed_size,
        })
    }
}

/// A pooled vector whose writable region is capped at the planned prefix size, so zstd never
/// decodes past the last needed table even when the pooled buffer is larger.
struct PrefixBuffer<'vec> {
    vec: &'vec mut Vec<u8>,
    limit: usize,
}

impl<'vec> PrefixBuffer<'vec> {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A buffer over `vec` capped at `limit` bytes.
    ///
    /// # Panics
    ///
    /// Panics if `limit` exceeds the capacity of `vec` ([`DecodePlan::new`] checks it).
    fn new(vec: &'vec mut Vec<u8>, limit: usize) -> Self {
        assert!(
            limit <= vec.capacity(),
            "the prefix limit must fit the buffer capacity"
        );
        Self { vec, limit }
    }
}

// SAFETY: `capacity` returns `limit`, which `new` checked is within the vector's allocation, and
// `as_mut_ptr` points at that allocation, so zstd writes only inside it. `filled_until` is only
// called with a length zstd reports as written, which is at most `limit`.
unsafe impl WriteBuf for PrefixBuffer<'_> {
    fn as_slice(&self) -> &[u8] {
        self.vec.as_slice()
    }

    fn capacity(&self) -> usize {
        self.limit
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.vec.as_mut_ptr()
    }

    unsafe fn filled_until(&mut self, n: usize) {
        // SAFETY: the caller guarantees that the first `n <= limit <= capacity` bytes are
        // initialized.
        unsafe { self.vec.set_len(n) };
    }
}

/// # Returns
///
/// The corruption reason for the zstd error `code`.
fn zstd_reason(code: zstd_safe::ErrorCode) -> CorruptionReason {
    CorruptionReason::Zstd(zstd_safe::get_error_name(code).into())
}

#[cfg(test)]
mod tests {
    use clp_s_resources::BufferPool;
    use clp_s_resources::BufferPoolConfig;
    use clp_s_resources::ErrorKind;
    use clp_s_resources::MemoryBudget;
    use clp_s_resources::PooledBuffer;
    use clp_s_resources::ResourceError;

    use super::StreamDecoder;
    use super::TableBytes;
    use crate::ByteRange;
    use crate::CorruptionReason;
    use crate::FormatError;
    use crate::FormatLocation;
    use crate::PackedStreamInfo;
    use crate::ReaderLimits;
    use crate::SchemaId;
    use crate::SectionId;
    use crate::SharedBytes;
    use crate::StreamId;
    use crate::StreamRead;
    use crate::TableInfo;
    use crate::TableOrdinal;

    const STREAM_SIZE: usize = 300_000;

    /// # Returns
    ///
    /// A compressible test stream of [`STREAM_SIZE`] bytes.
    fn content() -> Vec<u8> {
        (0..STREAM_SIZE)
            .map(|index| u8::try_from((index * 7 + index / 1000) % 251).expect("below 251"))
            .collect()
    }

    /// # Returns
    ///
    /// `bytes` as one zstd frame.
    fn compress(bytes: &[u8]) -> Vec<u8> {
        zstd::encode_all(bytes, 3).expect("zstd compresses")
    }

    /// # Returns
    ///
    /// A pooled buffer with room for `capacity` bytes.
    fn buffer(capacity: u64) -> PooledBuffer {
        BufferPool::new(BufferPoolConfig::default())
            .acquire(
                capacity,
                MemoryBudget::new(None)
                    .try_reserve(capacity)
                    .expect("the budget is unlimited"),
            )
            .expect("the buffer can be allocated")
    }

    /// # Returns
    ///
    /// A plan to decode `decoded_prefix_size` bytes of a stream of `uncompressed_size` bytes
    /// stored as `frame`.
    fn stream_read(frame: &[u8], uncompressed_size: u64, decoded_prefix_size: u64) -> StreamRead {
        StreamRead {
            stream: PackedStreamInfo {
                id: StreamId::new(4),
                compressed_range: ByteRange {
                    begin: 100,
                    end: 100 + frame.len() as u64,
                },
                uncompressed_size,
            },
            decoded_prefix_size,
            tables: vec![],
        }
    }

    /// # Returns
    ///
    /// The corruption reason of `error`, which must be located in the test stream.
    fn corruption_reason(error: FormatError) -> CorruptionReason {
        match error {
            FormatError::Corrupt { location, reason } => {
                assert_eq!(location, FormatLocation::PackedStream(StreamId::new(4)));
                *reason
            }
            other => panic!("expected a corruption error, got {other:?}"),
        }
    }

    /// Decodes `read` by pushing `frame` in chunks of `chunk_size` bytes.
    ///
    /// # Returns
    ///
    /// The decoded prefix on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`StreamDecoder::begin`]'s return values on failure.
    /// * Forwards [`super::StreamDecodeSession::push_chunk`]'s return values on failure.
    /// * Forwards [`super::StreamDecodeSession::finish`]'s return values on failure.
    fn decode_in_chunks(
        decoder: &mut StreamDecoder,
        frame: &[u8],
        read: &StreamRead,
        chunk_size: usize,
    ) -> Result<SharedBytes, FormatError> {
        let mut session = decoder.begin(read, buffer(read.decoded_prefix_size))?;
        for chunk in frame.chunks(chunk_size) {
            if session.push_chunk(chunk)? {
                break;
            }
        }
        session.finish()
    }

    #[test]
    fn decodes_whole_streams_and_prefixes() -> anyhow::Result<()> {
        let content = content();
        let frame = compress(&content);
        let mut decoder = StreamDecoder::new()?;
        for prefix_size in [STREAM_SIZE, 1, 1000, 131_072, 200_001] {
            let read = stream_read(&frame, STREAM_SIZE as u64, prefix_size as u64);
            let resident = decoder.decode_resident(&frame, &read, buffer(STREAM_SIZE as u64))?;
            assert_eq!(
                &resident[..],
                &content[..prefix_size],
                "prefix {prefix_size}"
            );
            for chunk_size in [1, 4096, frame.len()] {
                let chunked = decode_in_chunks(&mut decoder, &frame, &read, chunk_size)?;
                assert_eq!(
                    chunked, resident,
                    "prefix {prefix_size}, {chunk_size}-byte chunks"
                );
            }
        }
        let empty = decoder.decode_resident(&frame, &stream_read(&frame, 10, 0), buffer(0))?;
        assert_eq!(&empty[..], b"");
        Ok(())
    }

    #[test]
    fn frames_must_fill_their_range_exactly() -> anyhow::Result<()> {
        let content = content();
        let mut frame = compress(&content);
        frame.extend_from_slice(&[0; 4]);
        let read = stream_read(&frame, STREAM_SIZE as u64, STREAM_SIZE as u64);
        let mut decoder = StreamDecoder::new()?;
        let error = decoder
            .decode_resident(&frame, &read, buffer(STREAM_SIZE as u64))
            .expect_err("trailing bytes follow the frame");
        assert_eq!(
            corruption_reason(error),
            CorruptionReason::FrameSizeMismatch
        );

        let error = decode_in_chunks(&mut decoder, &frame, &read, 4096)
            .expect_err("trailing bytes follow the frame");
        assert_eq!(
            corruption_reason(error),
            CorruptionReason::FrameSizeMismatch
        );

        let truncated = &frame[..frame.len() - 100];
        let read = stream_read(truncated, STREAM_SIZE as u64, STREAM_SIZE as u64);
        assert!(
            decoder
                .decode_resident(truncated, &read, buffer(STREAM_SIZE as u64))
                .is_err()
        );
        assert!(decode_in_chunks(&mut decoder, truncated, &read, 4096).is_err());
        Ok(())
    }

    #[test]
    fn streams_must_decode_to_their_recorded_size() -> anyhow::Result<()> {
        let content = content();
        let frame = compress(&content);
        let mut decoder = StreamDecoder::new()?;
        for recorded_size in [STREAM_SIZE - 1, STREAM_SIZE + 1] {
            let read = stream_read(&frame, recorded_size as u64, recorded_size as u64);
            assert!(
                decoder
                    .decode_resident(&frame, &read, buffer(recorded_size as u64))
                    .is_err(),
                "a recorded size of {recorded_size} must be rejected"
            );
            assert!(
                decode_in_chunks(&mut decoder, &frame, &read, 4096).is_err(),
                "a recorded size of {recorded_size} must be rejected in chunks"
            );
        }
        let read = stream_read(&frame, STREAM_SIZE as u64 + 10, STREAM_SIZE as u64 + 5);
        let error = decoder
            .decode_resident(&frame, &read, buffer(STREAM_SIZE as u64 + 5))
            .expect_err("the stream ends before the prefix");
        assert_eq!(
            corruption_reason(error),
            CorruptionReason::DecodedSizeMismatch {
                expected_size: STREAM_SIZE as u64 + 5,
                actual_size: STREAM_SIZE as u64,
            }
        );
        Ok(())
    }

    #[test]
    fn invalid_plans_and_small_buffers_are_rejected() -> anyhow::Result<()> {
        let frame = compress(&content());
        let mut decoder = StreamDecoder::new()?;
        let read = stream_read(&frame, 10, 11);
        let error = decoder
            .decode_resident(&frame, &read, buffer(11))
            .expect_err("the prefix exceeds the stream");
        assert!(matches!(error, FormatError::Corrupt { .. }), "{error:?}");

        let read = stream_read(&frame, STREAM_SIZE as u64, STREAM_SIZE as u64);
        let small = BufferPool::new(BufferPoolConfig::default())
            .acquire(16, MemoryBudget::new(None).try_reserve(16)?)?;
        let capacity = {
            let mut probe = small;
            let capacity = probe.as_mut_vec().capacity();
            assert!(capacity < STREAM_SIZE, "the pool returned {capacity} bytes");
            decoder
                .begin(&read, probe)
                .expect_err("the buffer is too small");
            capacity
        };
        let error = decoder
            .decode_resident(&frame, &read, buffer(16))
            .expect_err("the buffer is too small");
        assert!(
            matches!(error, FormatError::LimitExceeded { limit_size, .. } if limit_size >= 16),
            "{error:?} (capacity {capacity})"
        );
        Ok(())
    }

    #[test]
    fn chunks_after_the_frame_ends_are_corrupt() -> anyhow::Result<()> {
        let content = content();
        let frame = compress(&content);
        let read = stream_read(&frame, STREAM_SIZE as u64, STREAM_SIZE as u64);
        let mut decoder = StreamDecoder::new()?;
        let mut session = decoder.begin(&read, buffer(STREAM_SIZE as u64))?;
        assert!(session.push_chunk(&frame)?);
        assert!(
            session.push_chunk(b"more")?,
            "a finished session ignores further chunks"
        );
        assert_eq!(&session.finish()?[..], content.as_slice());

        let mut session = decoder.begin(&read, buffer(STREAM_SIZE as u64))?;
        assert!(!session.push_chunk(&frame[..100])?);
        let error = session.finish().expect_err("the frame is incomplete");
        assert!(
            matches!(
                corruption_reason(error),
                CorruptionReason::DecodedSizeMismatch { .. }
            ),
            "finishing early is an error"
        );
        Ok(())
    }

    #[test]
    fn sections_decode_under_the_limit() -> anyhow::Result<()> {
        let content = content();
        let frame = compress(&content);
        let mut decoder = StreamDecoder::new()?;
        let location = FormatLocation::Section(SectionId::SchemaTree);
        let section = decoder.decode_section(&frame, location, &ReaderLimits::default())?;
        assert_eq!(&section[..], content.as_slice());

        let limits = ReaderLimits {
            max_section_decoded_size: 1000,
            ..ReaderLimits::default()
        };
        let error = decoder
            .decode_section(&frame, location, &limits)
            .expect_err("the bound exceeds 1000 bytes");
        assert!(
            matches!(error, FormatError::LimitExceeded { .. }),
            "{error:?}"
        );

        for invalid in [
            &frame[..frame.len() - 1],
            &[0x28, 0xb5, 0x2f][..],
            b"not zstd",
        ] {
            let error = decoder
                .decode_section(invalid, location, &ReaderLimits::default())
                .expect_err("the frame is invalid");
            assert!(
                matches!(
                    error,
                    FormatError::Corrupt {
                        location: error_location,
                        ..
                    } if error_location == location
                ),
                "{error:?}"
            );
        }
        let mut trailing = frame;
        trailing.push(0);
        let error = decoder
            .decode_section(&trailing, location, &ReaderLimits::default())
            .expect_err("a byte follows the frame");
        assert!(
            matches!(
                &error,
                FormatError::Corrupt { reason, .. }
                    if **reason == CorruptionReason::FrameSizeMismatch
            ),
            "{error:?}"
        );
        Ok(())
    }

    #[test]
    fn unallocatable_section_buffers_are_resource_errors() -> anyhow::Result<()> {
        const CONTENT_SIZE: u64 = 1 << 62;
        // A single-segment frame declaring an 8-byte content size, followed by one empty, last,
        // raw block.
        let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0xe0];
        frame.extend_from_slice(&CONTENT_SIZE.to_le_bytes());
        frame.extend_from_slice(&[0x01, 0x00, 0x00]);
        let limits = ReaderLimits {
            max_section_decoded_size: u64::MAX,
            ..ReaderLimits::default()
        };
        let error = StreamDecoder::new()?
            .decode_section(
                &frame,
                FormatLocation::Section(SectionId::SchemaTree),
                &limits,
            )
            .expect_err("a 4 EiB buffer cannot be allocated");
        assert!(
            matches!(
                error,
                FormatError::Resource(ResourceError::AllocationFailed { size: CONTENT_SIZE })
            ),
            "{error:?}"
        );
        assert_eq!(error.kind(), ErrorKind::ResourceExhausted);
        Ok(())
    }

    #[test]
    fn table_bytes_must_lie_inside_the_stream() -> anyhow::Result<()> {
        let stream = SharedBytes::from_static(b"0123456789");
        let info = TableInfo {
            ordinal: TableOrdinal::new(2),
            schema_id: SchemaId::new(0),
            stream_id: StreamId::new(0),
            stream_offset: 3,
            uncompressed_size: 4,
            num_rows: 1,
            first_record_ordinal: 0,
        };
        let table = TableBytes::new(&stream, info)?;
        assert_eq!(table.as_bytes(), b"3456");
        assert_eq!(table.info(), &info);
        assert_eq!(table.shared_bytes().as_ptr(), stream[3..].as_ptr());

        for (stream_offset, uncompressed_size) in [(3, 8), (11, 0), (u64::MAX, 2)] {
            let error = TableBytes::new(
                &stream,
                TableInfo {
                    stream_offset,
                    uncompressed_size,
                    ..info
                },
            )
            .expect_err("the table is outside the stream");
            assert!(
                matches!(
                    &error,
                    FormatError::Corrupt { location: FormatLocation::Table(ordinal), reason }
                        if *ordinal == TableOrdinal::new(2)
                            && **reason == CorruptionReason::TableOutsideStream
                ),
                "{error:?}"
            );
        }
        Ok(())
    }
}
