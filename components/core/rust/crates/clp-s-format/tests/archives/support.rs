//! Shared helpers of the archive tests: fixture discovery, a driver of the sans-IO opener, a full
//! read of every structure, and an archive rebuilder for structured mutations.

use std::path::Path;
use std::path::PathBuf;

use clp_s_format::ArchiveHeader;
use clp_s_format::ArchiveIndex;
use clp_s_format::ArchiveOpener;
use clp_s_format::DictionaryNeeds;
use clp_s_format::FormatError;
use clp_s_format::LogTypeDictionary;
use clp_s_format::OpenOptions;
use clp_s_format::OpenStep;
use clp_s_format::SectionId;
use clp_s_format::SharedBytes;
use clp_s_format::StreamDecoder;
use clp_s_format::TableBytes;
use clp_s_format::TableSet;
use clp_s_format::VariableDictionary;
use clp_s_resources::BufferPool;
use clp_s_resources::BufferPoolConfig;
use clp_s_resources::MemoryBudget;

/// The metadata packet type of the file-info packet.
const FILE_INFO_PACKET_TYPE: u8 = 1;

/// # Returns
///
/// The fixture corpus directory (`tests/fixtures` of the prototype workspace).
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

/// # Returns
///
/// Every readable fixture archive, sorted by file name.
pub fn readable_archives() -> Vec<PathBuf> {
    let mut archives: Vec<PathBuf> = std::fs::read_dir(fixtures_dir().join("archives"))
        .expect("the fixture archive directory is readable")
        .map(|entry| entry.expect("the directory entry is readable").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "clps")
        })
        .collect();
    archives.sort();
    archives
}

/// # Returns
///
/// The bytes of the file at `path`.
pub fn read_bytes(path: &Path) -> SharedBytes {
    SharedBytes::from(std::fs::read(path).expect("the fixture is readable"))
}

/// Drives the opener over in-memory archive bytes.
///
/// # Returns
///
/// The archive index on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`ArchiveOpener::advance`]'s return values on failure.
/// * Forwards [`ArchiveOpener::push_range`]'s return values on failure.
/// * Forwards [`ArchiveOpener::into_index`]'s return values on failure.
///
/// # Panics
///
/// Panics if the opener requests bytes outside the archive or does not converge.
pub fn open_index(bytes: &SharedBytes, options: OpenOptions) -> Result<ArchiveIndex, FormatError> {
    let mut opener = ArchiveOpener::new(options, bytes.len() as u64);
    for _ in 0..8 {
        match opener.advance()? {
            OpenStep::NeedsData(ranges) => {
                for range in ranges {
                    let begin = usize::try_from(range.begin).expect("the range fits in memory");
                    let end = usize::try_from(range.end).expect("the range fits in memory");
                    assert!(
                        begin <= end && end <= bytes.len(),
                        "the opener requested {range:?} outside a file of {} bytes",
                        bytes.len()
                    );
                    opener.push_range(range, bytes.slice(begin..end))?;
                }
            }
            OpenStep::MetadataReady => {
                assert!(
                    opener.metadata().is_some(),
                    "the metadata checkpoint has metadata"
                );
            }
            OpenStep::IndexReady => return opener.into_index(),
        }
    }
    panic!("the opener did not reach the index checkpoint");
}

/// Everything a full read of an archive produced.
pub struct ArchiveContents {
    pub index: ArchiveIndex,
    pub variables: VariableDictionary,
    pub logtypes: LogTypeDictionary,
    pub arrays: LogTypeDictionary,
    pub decoded_streams: Vec<SharedBytes>,
    pub tables: Vec<TableBytes>,
}

/// Opens an archive and reads every structure: the dictionaries, every packed stream (decoded in
/// full, single-shot and in `chunk_size` chunks, which must agree) and every table. Streams above
/// [`clp_s_format::ReaderLimits::max_packed_stream_size`] are refused, as storage does.
///
/// # Returns
///
/// The contents on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if a stream exceeds the size limit or its buffer cannot be allocated.
/// * Forwards [`open_index`]'s return values on failure.
/// * Forwards [`VariableDictionary::load`]'s return values on failure.
/// * Forwards [`LogTypeDictionary::load`]'s return values on failure.
/// * Forwards [`LogTypeDictionary::load_array`]'s return values on failure.
/// * Forwards [`StreamDecoder::decode_resident`]'s return values on failure.
/// * Forwards [`StreamDecoder::begin`]'s return values on failure.
/// * Forwards [`clp_s_format::StreamDecodeSession::push_chunk`]'s return values on failure.
/// * Forwards [`clp_s_format::StreamDecodeSession::finish`]'s return values on failure.
/// * Forwards [`TableBytes::new`]'s return values on failure.
///
/// # Panics
///
/// Panics if the single-shot and chunked decodes of a stream differ.
pub fn read_everything(
    bytes: &SharedBytes,
    options: OpenOptions,
    chunk_size: usize,
) -> anyhow::Result<ArchiveContents> {
    let index = open_index(bytes, options)?;
    let section = |section: SectionId| {
        let range = index
            .metadata()
            .sections()
            .range(section)
            .expect("every section exists after open");
        bytes.slice(to_usize(range.begin)..to_usize(range.end))
    };
    let variables =
        VariableDictionary::load(&section(SectionId::VariableDictionary), &options.limits)?;
    let logtypes =
        LogTypeDictionary::load(&section(SectionId::LogTypeDictionary), &options.limits)?;
    let arrays =
        LogTypeDictionary::load_array(&section(SectionId::ArrayDictionary), &options.limits)?;

    let num_tables = u32::try_from(index.tables().tables().len()).expect("few tables");
    let plan = index.plan_reads(&TableSet::full(num_tables), DictionaryNeeds::default());
    let pool = BufferPool::new(BufferPoolConfig::default());
    let budget = MemoryBudget::new(None);
    let mut decoder = StreamDecoder::new()?;
    let mut decoded_streams = Vec::new();
    let mut tables = Vec::new();
    for read in &plan.streams {
        anyhow::ensure!(
            read.stream.uncompressed_size <= options.limits.max_packed_stream_size,
            "stream {:?} exceeds the size limit",
            read.stream.id
        );
        // Each decode gets a buffer pre-filled with a different stale byte, so a decoded byte that
        // depends on leftover buffer contents makes the two decodes differ on every run. The fill
        // is capped because mutated sizes plan huge buffers whose decodes fail anyway, and touching
        // them all would dominate the run time.
        let acquire = |stale_byte: u8| -> anyhow::Result<_> {
            const MAX_STALE_FILL_SIZE: usize = 1 << 20;
            let reservation = budget.try_reserve(read.decoded_prefix_size)?;
            let mut buffer = pool.acquire(read.decoded_prefix_size, reservation)?;
            let vec = buffer.as_mut_vec();
            vec.resize(vec.capacity().min(MAX_STALE_FILL_SIZE), stale_byte);
            Ok(buffer)
        };
        let range = read.stream.compressed_range;
        let frame = bytes.slice(to_usize(range.begin)..to_usize(range.end));
        let single_shot = decoder.decode_resident(&frame, read, acquire(0x00)?)?;

        let mut session = decoder.begin(read, acquire(0xff)?)?;
        for chunk in frame.chunks(chunk_size.max(1)) {
            if session.push_chunk(chunk)? {
                break;
            }
        }
        let chunked = session.finish()?;
        if single_shot != chunked {
            let first_difference = single_shot
                .iter()
                .zip(chunked.iter())
                .position(|(single_shot_byte, chunked_byte)| single_shot_byte != chunked_byte);
            let fresh_decode = zstd::decode_all(&frame[..]).ok();
            let matches_fresh_decode = |decoded: &SharedBytes| {
                fresh_decode
                    .as_ref()
                    .and_then(|fresh_decode| fresh_decode.get(..decoded.len()))
                    == Some(&decoded[..])
            };
            panic!(
                "single-shot and chunked decodes differ: first differing offset \
                 {first_difference:?}, sizes {} and {}, matches of a fresh decode {} and {}, read \
                 plan {read:?}",
                single_shot.len(),
                chunked.len(),
                matches_fresh_decode(&single_shot),
                matches_fresh_decode(&chunked)
            );
        }

        for ordinal in &read.tables {
            let info = *index
                .tables()
                .table(*ordinal)
                .expect("planned tables exist");
            tables.push(TableBytes::new(&single_shot, info)?);
        }
        decoded_streams.push(single_shot);
    }
    Ok(ArchiveContents {
        index,
        variables,
        logtypes,
        arrays,
        decoded_streams,
        tables,
    })
}

/// # Returns
///
/// `value` as a `usize`.
pub fn to_usize(value: u64) -> usize {
    usize::try_from(value).expect("the value fits in memory")
}

/// An archive taken apart into header fields, metadata packets and raw sections, so tests can
/// change one structure and reassemble a consistent archive (sizes and offsets recomputed).
#[derive(Clone)]
pub struct ArchiveParts {
    /// The magic number and the packed version.
    pub header_prefix: [u8; 8],
    pub uncompressed_size: u64,
    pub reserved: [u8; 32],
    pub compression_type: u16,

    /// Every metadata packet in file order; the file-info payload is regenerated on encode.
    pub packets: Vec<(u8, Vec<u8>)>,

    /// Every section's raw bytes, in file order.
    pub sections: Vec<(SectionId, Vec<u8>)>,
}

impl ArchiveParts {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The parts of the valid archive `bytes`.
    pub fn from_archive(bytes: &SharedBytes) -> Self {
        let mut opener = ArchiveOpener::new(OpenOptions::default(), bytes.len() as u64);
        let mut sections = Vec::new();
        loop {
            match opener.advance().expect("the source archive is valid") {
                OpenStep::NeedsData(ranges) => {
                    for range in ranges {
                        opener
                            .push_range(
                                range,
                                bytes.slice(to_usize(range.begin)..to_usize(range.end)),
                            )
                            .expect("the range is complete");
                    }
                }
                OpenStep::MetadataReady => {
                    let metadata = opener.metadata().expect("the metadata is decoded");
                    for section in SectionId::ALL {
                        let range = metadata
                            .sections()
                            .range(section)
                            .expect("every section exists");
                        sections.push((
                            range.begin,
                            section,
                            bytes[to_usize(range.begin)..to_usize(range.end)].to_vec(),
                        ));
                    }
                    break;
                }
                OpenStep::IndexReady => panic!("the metadata checkpoint comes first"),
            }
        }
        sections.sort_by_key(|(begin, ..)| *begin);

        let header = &bytes[..to_usize(ArchiveHeader::SIZE)];
        let metadata_section_size =
            u32::from_le_bytes(header[56..60].try_into().expect("four bytes"));
        let frame = &bytes[64..64 + to_usize(metadata_section_size.into())];
        let decoded = zstd::decode_all(frame).expect("the metadata frame decodes");
        let mut packets = Vec::new();
        let mut position = 1;
        for _ in 0..decoded[0] {
            let packet_type = decoded[position];
            let size = u32::from_le_bytes(
                decoded[position + 1..position + 5]
                    .try_into()
                    .expect("four bytes"),
            );
            let begin = position + 5;
            let end = begin + to_usize(size.into());
            packets.push((packet_type, decoded[begin..end].to_vec()));
            position = end;
        }

        Self {
            header_prefix: header[..8].try_into().expect("eight bytes"),
            uncompressed_size: u64::from_le_bytes(header[8..16].try_into().expect("eight bytes")),
            reserved: header[24..56].try_into().expect("32 bytes"),
            compression_type: u16::from_le_bytes(header[60..62].try_into().expect("two bytes")),
            packets,
            sections: sections
                .into_iter()
                .map(|(_, section, raw)| (section, raw))
                .collect(),
        }
    }

    /// # Returns
    ///
    /// The decoded payload of `section` (for a dictionary, the frame after the entry count).
    pub fn decoded_section(&self, section: SectionId) -> Vec<u8> {
        let raw = self.raw_section(section);
        let frame = if is_dictionary(section) {
            &raw[8..]
        } else {
            raw
        };
        if frame.is_empty() {
            return Vec::new();
        }
        zstd::decode_all(frame).expect("the section frame decodes")
    }

    /// Replaces the decoded payload of `section`, re-compressing it (a dictionary keeps its count).
    pub fn set_decoded_section(&mut self, section: SectionId, decoded: &[u8]) {
        let frame = zstd::encode_all(decoded, 3).expect("zstd compresses");
        let raw = self.raw_section_mut(section);
        if is_dictionary(section) {
            raw.truncate(8);
            raw.extend_from_slice(&frame);
        } else {
            *raw = frame;
        }
    }

    /// # Returns
    ///
    /// The raw bytes of `section`.
    pub fn raw_section(&self, section: SectionId) -> &[u8] {
        &self
            .sections
            .iter()
            .find(|(id, _)| *id == section)
            .expect("every section exists")
            .1
    }

    /// # Returns
    ///
    /// The raw bytes of `section`, mutably.
    pub fn raw_section_mut(&mut self, section: SectionId) -> &mut Vec<u8> {
        &mut self
            .sections
            .iter_mut()
            .find(|(id, _)| *id == section)
            .expect("every section exists")
            .1
    }

    /// # Returns
    ///
    /// The payload of the first packet of type `packet_type`, mutably.
    pub fn packet_mut(&mut self, packet_type: u8) -> &mut Vec<u8> {
        &mut self
            .packets
            .iter_mut()
            .find(|(candidate, _)| *candidate == packet_type)
            .expect("the packet exists")
            .1
    }

    /// # Returns
    ///
    /// The decoded metadata section, with the file-info packet regenerated from the sections.
    pub fn decoded_metadata(&self) -> Vec<u8> {
        let mut decoded = vec![u8::try_from(self.packets.len()).expect("few packets")];
        for (packet_type, payload) in &self.packets {
            let file_info;
            let payload = if FILE_INFO_PACKET_TYPE == *packet_type {
                file_info = self.encode_file_info();
                &file_info
            } else {
                payload
            };
            decoded.push(*packet_type);
            decoded.extend_from_slice(
                &u32::try_from(payload.len())
                    .expect("small payload")
                    .to_le_bytes(),
            );
            decoded.extend_from_slice(payload);
        }
        decoded
    }

    /// # Returns
    ///
    /// The reassembled archive.
    pub fn encode(&self) -> SharedBytes {
        self.encode_with_metadata(&self.decoded_metadata())
    }

    /// # Returns
    ///
    /// The reassembled archive with `decoded_metadata` as its metadata section.
    pub fn encode_with_metadata(&self, decoded_metadata: &[u8]) -> SharedBytes {
        let metadata = zstd::encode_all(decoded_metadata, 3).expect("zstd compresses");
        let sections_size: usize = self.sections.iter().map(|(_, raw)| raw.len()).sum();
        let compressed_size = 64 + metadata.len() + sections_size;
        let mut archive = Vec::with_capacity(compressed_size);
        archive.extend_from_slice(&self.header_prefix);
        archive.extend_from_slice(&self.uncompressed_size.to_le_bytes());
        archive.extend_from_slice(&(compressed_size as u64).to_le_bytes());
        archive.extend_from_slice(&self.reserved);
        archive.extend_from_slice(
            &u32::try_from(metadata.len())
                .expect("small metadata")
                .to_le_bytes(),
        );
        archive.extend_from_slice(&self.compression_type.to_le_bytes());
        archive.extend_from_slice(&[0, 0]);
        archive.extend_from_slice(&metadata);
        for (_, raw) in &self.sections {
            archive.extend_from_slice(raw);
        }
        SharedBytes::from(archive)
    }

    /// # Returns
    ///
    /// The file-info packet (`{"files": [{"n": name, "o": offset}]}`) for the current sections.
    fn encode_file_info(&self) -> Vec<u8> {
        let mut encoded = vec![0x81, 0xa5];
        encoded.extend_from_slice(b"files");
        encoded.push(0x90 | u8::try_from(self.sections.len()).expect("few sections"));
        let mut offset: u64 = 0;
        for (section, raw) in &self.sections {
            let name = section.name().as_bytes();
            encoded.extend_from_slice(&[0x82, 0xa1, b'n']);
            encoded.push(0xa0 | u8::try_from(name.len()).expect("short name"));
            encoded.extend_from_slice(name);
            encoded.extend_from_slice(&[0xa1, b'o', 0xcf]);
            encoded.extend_from_slice(&offset.to_be_bytes());
            offset += raw.len() as u64;
        }
        encoded
    }
}

/// # Returns
///
/// Whether `section` is a dictionary (a raw entry count before its frame).
const fn is_dictionary(section: SectionId) -> bool {
    matches!(
        section,
        SectionId::VariableDictionary | SectionId::LogTypeDictionary | SectionId::ArrayDictionary
    )
}
