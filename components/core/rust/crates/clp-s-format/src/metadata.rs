//! The metadata section: archive info, the file-info packet (section table), the timestamp
//! dictionary and the range index.

use crate::ArchiveHeader;
use crate::ByteRange;
use crate::ClassifiedVersion;
use crate::CorruptionReason;
use crate::FormatError;
use crate::FormatLocation;
use crate::RangeIndex;
use crate::SharedBytes;
use crate::TimestampIndex;
use crate::UnsupportedFeature;
use crate::bytes_reader::ByteReader;
use crate::msgpack;
use crate::msgpack::Item;

/// A section of a single-file archive, as named in the file-info packet.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SectionId {
    /// `/schema_tree`.
    SchemaTree,

    /// `/schema_ids`.
    SchemaMap,

    /// `/table_metadata`.
    TableMetadata,

    /// `/var.dict`.
    VariableDictionary,

    /// `/log.dict`.
    LogTypeDictionary,

    /// `/array.dict`.
    ArrayDictionary,

    /// `/0`, the packed streams.
    PackedStreams,
}

impl SectionId {
    /// Every section, in the order the C++ writer stores them.
    pub const ALL: [Self; 7] = [
        Self::SchemaTree,
        Self::SchemaMap,
        Self::TableMetadata,
        Self::VariableDictionary,
        Self::LogTypeDictionary,
        Self::ArrayDictionary,
        Self::PackedStreams,
    ];

    /// # Returns
    ///
    /// The section for the file-info name `name`, or `None` if the name is unknown.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|section| section.name() == name)
    }

    /// # Returns
    ///
    /// The file-info name of the section (e.g. `/schema_tree`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::SchemaTree => "/schema_tree",
            Self::SchemaMap => "/schema_ids",
            Self::TableMetadata => "/table_metadata",
            Self::VariableDictionary => "/var.dict",
            Self::LogTypeDictionary => "/log.dict",
            Self::ArrayDictionary => "/array.dict",
            Self::PackedStreams => "/0",
        }
    }

    /// # Returns
    ///
    /// The position of the section in [`Self::ALL`].
    const fn index(self) -> usize {
        match self {
            Self::SchemaTree => 0,
            Self::SchemaMap => 1,
            Self::TableMetadata => 2,
            Self::VariableDictionary => 3,
            Self::LogTypeDictionary => 4,
            Self::ArrayDictionary => 5,
            Self::PackedStreams => 6,
        }
    }
}

/// Absolute ranges of the archive's sections, validated from the file-info packet.
#[derive(Clone, Debug)]
pub struct SectionTable {
    ranges: [Option<ByteRange>; SectionId::ALL.len()],
}

impl SectionTable {
    /// # Returns
    ///
    /// The absolute range of `section`, or `None` if the archive has no such section.
    #[must_use]
    pub const fn range(&self, section: SectionId) -> Option<ByteRange> {
        self.ranges[section.index()]
    }

    /// Factory function.
    ///
    /// Parses the file-info packet (`{files: [{n, o}]}`). As in C++, a section ends where the
    /// next listed file starts, and the last one ends at the header's compressed size.
    ///
    /// # Returns
    ///
    /// The validated section table on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if the packet is malformed, a range is invalid, a known section
    ///   is listed twice, or a section is missing.
    /// * Forwards [`msgpack::read_item`]'s return values on failure.
    /// * Forwards [`msgpack::skip_value`]'s return values on failure.
    /// * Forwards [`msgpack::skip_children`]'s return values on failure.
    pub(crate) fn parse(payload: &[u8], header: &ArchiveHeader) -> Result<Self, FormatError> {
        let mut reader = ByteReader::new(
            payload,
            FormatLocation::MetadataPacket {
                packet_type: PacketType::FILE_INFO,
            },
        );
        let invalid =
            |reader: &ByteReader<'_>| reader.corrupt(CorruptionReason::InvalidSectionTable);

        // C++ reads the value of the map's first entry, whatever its key.
        let Item::Map(num_entries) = msgpack::read_item(&mut reader)? else {
            return Err(invalid(&reader));
        };
        if 0 == num_entries {
            return Err(invalid(&reader));
        }
        msgpack::skip_value(&mut reader)?;
        let Item::Array(num_files) = msgpack::read_item(&mut reader)? else {
            return Err(invalid(&reader));
        };

        let mut files: Vec<(&[u8], u64)> =
            Vec::with_capacity(reader.check_count(num_files.into(), 1)?);
        for _ in 0..num_files {
            let Item::Map(num_fields) = msgpack::read_item(&mut reader)? else {
                return Err(invalid(&reader));
            };
            let mut name = None;
            let mut offset = None;
            for _ in 0..num_fields {
                let Item::String(key) = msgpack::read_item(&mut reader)? else {
                    return Err(invalid(&reader));
                };
                match (key, msgpack::read_item(&mut reader)?) {
                    (b"n", Item::String(value)) => name = Some(value),
                    (b"o", Item::UnsignedInteger(value)) => offset = Some(value),
                    (b"n" | b"o", _) => return Err(invalid(&reader)),
                    (_, value) => msgpack::skip_children(&mut reader, value)?,
                }
            }
            let (Some(name), Some(offset)) = (name, offset) else {
                return Err(invalid(&reader));
            };
            files.push((name, offset));
        }

        let sections_offset = header.sections_offset();
        let mut ranges = [None; SectionId::ALL.len()];
        for (index, (name, offset)) in files.iter().enumerate() {
            let begin = sections_offset.checked_add(*offset);
            let end = match files.get(index + 1) {
                Some((_, next_offset)) => sections_offset.checked_add(*next_offset),
                None => Some(header.compressed_size),
            };
            let (Some(begin), Some(end)) = (begin, end) else {
                return Err(invalid(&reader));
            };
            // Each end is the next begin and the last end is `compressed_size`, so `begin <= end`
            // for every section also bounds every end by `compressed_size`.
            if begin > end {
                return Err(invalid(&reader));
            }
            let Some(section) = std::str::from_utf8(name)
                .ok()
                .and_then(SectionId::from_name)
            else {
                continue;
            };
            let slot = &mut ranges[section.index()];
            if slot.is_some() {
                return Err(invalid(&reader));
            }
            *slot = Some(ByteRange { begin, end });
        }

        for section in SectionId::ALL {
            if ranges[section.index()].is_none() {
                return Err(reader.corrupt(CorruptionReason::MissingSection(section)));
            }
        }
        Ok(Self { ranges })
    }
}

/// A metadata packet this reader does not understand, kept verbatim.
#[derive(Clone, Debug)]
pub struct UnknownPacket {
    pub packet_type: u8,
    pub payload: SharedBytes,
}

/// The header plus the decoded metadata packets: everything archive-level pruning needs.
#[derive(Debug)]
pub struct ArchiveMetadata {
    header: ArchiveHeader,
    version: ClassifiedVersion,
    sections: SectionTable,
    timestamp_index: TimestampIndex,
    range_index: Option<RangeIndex>,
    unknown_packets: Vec<UnknownPacket>,
}

impl ArchiveMetadata {
    /// # Returns
    ///
    /// The validated header.
    #[must_use]
    pub const fn header(&self) -> &ArchiveHeader {
        &self.header
    }

    /// # Returns
    ///
    /// The classified format version.
    #[must_use]
    pub const fn version(&self) -> ClassifiedVersion {
        self.version
    }

    /// # Returns
    ///
    /// The section table.
    #[must_use]
    pub const fn sections(&self) -> &SectionTable {
        &self.sections
    }

    /// # Returns
    ///
    /// The timestamp index (the timestamp dictionary packet).
    #[must_use]
    pub const fn timestamp_index(&self) -> &TimestampIndex {
        &self.timestamp_index
    }

    /// # Returns
    ///
    /// The range index, or `None` if the archive has none.
    #[must_use]
    pub const fn range_index(&self) -> Option<&RangeIndex> {
        self.range_index.as_ref()
    }

    /// # Returns
    ///
    /// The metadata packets of unknown types, in file order.
    #[must_use]
    pub fn unknown_packets(&self) -> &[UnknownPacket] {
        &self.unknown_packets
    }

    /// Factory function.
    ///
    /// Parses the decoded metadata section: a `u8` packet count, then per packet a `u8` type, a
    /// `u32` size and the payload. Each payload is parsed from an exact slice of its declared
    /// size; a zero-length packet is treated as absent.
    ///
    /// # Returns
    ///
    /// The metadata on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if a packet is shorter than declared, a known packet appears
    ///   twice, or the file-info packet is missing.
    /// * [`FormatError::UnsupportedFeature`] if the archive has more than one segment.
    /// * Forwards [`ClassifiedVersion::classify`]'s return values on failure.
    /// * Forwards [`ByteReader::read_u8`]'s return values on failure.
    /// * Forwards [`ByteReader::read_u32`]'s return values on failure.
    /// * Forwards [`parse_num_segments`]'s return values on failure.
    /// * Forwards [`SectionTable::parse`]'s return values on failure.
    /// * Forwards [`TimestampIndex::parse`]'s return values on failure.
    /// * Forwards [`RangeIndex::parse`]'s return values on failure.
    pub(crate) fn parse(header: ArchiveHeader, decoded: &SharedBytes) -> Result<Self, FormatError> {
        let version = ClassifiedVersion::classify(header.version)?;
        let mut reader = ByteReader::new(decoded, FormatLocation::MetadataSection);
        let num_packets = reader.read_u8()?;

        let mut has_archive_info = false;
        let mut sections = None;
        let mut timestamp_index = None;
        let mut range_index = None;
        let mut unknown_packets = Vec::new();
        for _ in 0..num_packets {
            let packet_type = reader.read_u8()?;
            let payload_size = reader.read_u32()?;
            let begin = reader.position();
            if reader.read_bytes(payload_size.into()).is_err() {
                return Err(FormatError::corrupt(
                    FormatLocation::MetadataPacket { packet_type },
                    CorruptionReason::ShortPayload,
                ));
            }
            let payload = decoded.slice(begin..reader.position());
            let duplicate = || {
                FormatError::corrupt(
                    FormatLocation::MetadataPacket { packet_type },
                    CorruptionReason::DuplicatePacket(packet_type),
                )
            };
            if payload.is_empty() && packet_type <= PacketType::RANGE_INDEX {
                continue;
            }
            match packet_type {
                PacketType::ARCHIVE_INFO => {
                    if has_archive_info {
                        return Err(duplicate());
                    }
                    has_archive_info = true;
                    let num_segments = parse_num_segments(&payload)?;
                    if 1 != num_segments {
                        return Err(FormatError::UnsupportedFeature {
                            location: FormatLocation::MetadataPacket { packet_type },
                            feature: UnsupportedFeature::NumSegments(num_segments),
                        });
                    }
                }
                PacketType::FILE_INFO => {
                    if sections.is_some() {
                        return Err(duplicate());
                    }
                    sections = Some(SectionTable::parse(&payload, &header)?);
                }
                PacketType::TIMESTAMP_DICTIONARY => {
                    if timestamp_index.is_some() {
                        return Err(duplicate());
                    }
                    timestamp_index = Some(TimestampIndex::parse(&payload, version)?);
                }
                PacketType::RANGE_INDEX => {
                    if range_index.is_some() {
                        return Err(duplicate());
                    }
                    range_index = Some(RangeIndex::parse(&payload)?);
                }
                _ => unknown_packets.push(UnknownPacket {
                    packet_type,
                    payload,
                }),
            }
        }

        let Some(sections) = sections else {
            return Err(FormatError::corrupt(
                FormatLocation::MetadataPacket {
                    packet_type: PacketType::FILE_INFO,
                },
                CorruptionReason::InvalidSectionTable,
            ));
        };
        Ok(Self {
            header,
            version,
            sections,
            timestamp_index: timestamp_index.unwrap_or_default(),
            range_index,
            unknown_packets,
        })
    }
}

/// Metadata packet type numbers (`ArchiveMetadataPacketType`).
pub struct PacketType;

impl PacketType {
    pub const ARCHIVE_INFO: u8 = 0;
    pub const FILE_INFO: u8 = 1;
    pub const RANGE_INDEX: u8 = 3;
    pub const TIMESTAMP_DICTIONARY: u8 = 2;
}

/// Reads `num_segments` from the archive-info packet (a msgpack map).
///
/// # Returns
///
/// The number of segments on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`FormatError::Corrupt`] if the packet is not a map with an unsigned `num_segments`.
/// * Forwards [`msgpack::read_item`]'s return values on failure.
/// * Forwards [`msgpack::skip_children`]'s return values on failure.
fn parse_num_segments(payload: &[u8]) -> Result<u64, FormatError> {
    let mut reader = ByteReader::new(
        payload,
        FormatLocation::MetadataPacket {
            packet_type: PacketType::ARCHIVE_INFO,
        },
    );
    let malformed = |reader: &ByteReader<'_>| reader.corrupt(CorruptionReason::MalformedMsgpack);
    let Item::Map(num_entries) = msgpack::read_item(&mut reader)? else {
        return Err(malformed(&reader));
    };
    let mut num_segments = None;
    for _ in 0..num_entries {
        let Item::String(key) = msgpack::read_item(&mut reader)? else {
            return Err(malformed(&reader));
        };
        match (key, msgpack::read_item(&mut reader)?) {
            (b"num_segments", Item::UnsignedInteger(value)) => num_segments = Some(value),
            (b"num_segments", _) => return Err(malformed(&reader)),
            (_, value) => msgpack::skip_children(&mut reader, value)?,
        }
    }
    num_segments.ok_or_else(|| malformed(&reader))
}

#[cfg(test)]
mod tests {
    use super::ArchiveMetadata;
    use super::PacketType;
    use super::SectionId;
    use super::SectionTable;
    use crate::ArchiveHeader;
    use crate::ByteRange;
    use crate::CorruptionReason;
    use crate::FormatError;
    use crate::FormatLocation;
    use crate::FormatVersion;
    use crate::SharedBytes;
    use crate::UnsupportedFeature;

    const HEADER: ArchiveHeader = ArchiveHeader {
        version: FormatVersion {
            major: 0,
            minor: 5,
            patch: 1,
        },
        uncompressed_size: 0,
        compressed_size: 1000,
        metadata_section_size: 36,
    };

    /// # Returns
    ///
    /// `text` encoded as a msgpack `str8`.
    fn str_item(text: &str) -> Vec<u8> {
        let mut bytes = vec![0xd9, u8::try_from(text.len()).expect("short test string")];
        bytes.extend_from_slice(text.as_bytes());
        bytes
    }

    /// # Returns
    ///
    /// `value` encoded as a msgpack `uint64`.
    fn uint_item(value: u64) -> Vec<u8> {
        let mut bytes = vec![0xcf];
        bytes.extend_from_slice(&value.to_be_bytes());
        bytes
    }

    /// # Returns
    ///
    /// The file-info packet listing `files` as `(name, offset)`.
    fn encode_file_info(files: &[(&str, u64)]) -> Vec<u8> {
        let mut bytes = vec![0x81];
        bytes.extend(str_item("files"));
        bytes.push(0xdc);
        bytes.extend_from_slice(&u16::try_from(files.len()).expect("few files").to_be_bytes());
        for (name, offset) in files {
            bytes.push(0x82);
            bytes.extend(str_item("n"));
            bytes.extend(str_item(name));
            bytes.extend(str_item("o"));
            bytes.extend(uint_item(*offset));
        }
        bytes
    }

    /// # Returns
    ///
    /// Every section, in writer order, 100 bytes apart.
    fn standard_files() -> Vec<(&'static str, u64)> {
        SectionId::ALL
            .into_iter()
            .zip((0_u64..).map(|index| index * 100))
            .map(|(section, offset)| (section.name(), offset))
            .collect()
    }

    /// # Returns
    ///
    /// The archive-info packet with `num_segments`.
    fn encode_archive_info(num_segments: u64) -> Vec<u8> {
        let mut bytes = vec![0x81];
        bytes.extend(str_item("num_segments"));
        bytes.extend(uint_item(num_segments));
        bytes
    }

    /// # Returns
    ///
    /// The decoded metadata section holding `packets`.
    fn encode_metadata(packets: &[(u8, Vec<u8>)]) -> SharedBytes {
        let mut bytes = vec![u8::try_from(packets.len()).expect("few packets")];
        for (packet_type, payload) in packets {
            bytes.push(*packet_type);
            bytes.extend_from_slice(
                &u32::try_from(payload.len())
                    .expect("small payload")
                    .to_le_bytes(),
            );
            bytes.extend_from_slice(payload);
        }
        SharedBytes::from(bytes)
    }

    /// # Returns
    ///
    /// An empty timestamp-dictionary payload.
    fn timestamp_dictionary() -> Vec<u8> {
        [0_u64.to_le_bytes(), 0_u64.to_le_bytes()].concat()
    }

    /// # Returns
    ///
    /// The location and reason of the corruption error `error`.
    fn corruption(error: FormatError) -> (FormatLocation, CorruptionReason) {
        match error {
            FormatError::Corrupt { location, reason } => (location, *reason),
            other => panic!("expected a corruption error, got {other:?}"),
        }
    }

    #[test]
    fn section_names_round_trip() {
        for section in SectionId::ALL {
            assert_eq!(SectionId::from_name(section.name()), Some(section));
        }
        assert_eq!(SectionId::from_name("/header"), None);
        assert_eq!(SectionId::from_name("schema_tree"), None);
    }

    #[test]
    fn sections_end_where_the_next_listed_file_starts() -> anyhow::Result<()> {
        let mut files = standard_files();
        files.insert(2, ("/unknown", 150));
        let table = SectionTable::parse(&encode_file_info(&files), &HEADER)?;
        let sections_offset = HEADER.sections_offset();
        assert_eq!(
            table.range(SectionId::SchemaTree),
            Some(ByteRange {
                begin: sections_offset,
                end: sections_offset + 100,
            })
        );
        assert_eq!(
            table.range(SectionId::SchemaMap),
            Some(ByteRange {
                begin: sections_offset + 100,
                end: sections_offset + 150,
            })
        );
        assert_eq!(
            table.range(SectionId::PackedStreams),
            Some(ByteRange {
                begin: sections_offset + 600,
                end: HEADER.compressed_size,
            })
        );
        Ok(())
    }

    #[test]
    fn rejects_invalid_section_tables() {
        let mut decreasing = standard_files();
        decreasing.swap(1, 2);
        let mut duplicate = standard_files();
        duplicate.push(("/0", 700));
        let mut beyond_file = standard_files();
        beyond_file[6].1 = 2000;
        let cases = [
            (
                encode_file_info(&decreasing),
                CorruptionReason::InvalidSectionTable,
            ),
            (
                encode_file_info(&duplicate),
                CorruptionReason::InvalidSectionTable,
            ),
            (
                encode_file_info(&beyond_file),
                CorruptionReason::InvalidSectionTable,
            ),
            (
                encode_file_info(&standard_files()[..6]),
                CorruptionReason::MissingSection(SectionId::PackedStreams),
            ),
            (vec![0x80], CorruptionReason::InvalidSectionTable),
            (vec![0x90], CorruptionReason::InvalidSectionTable),
        ];
        for (payload, expected_reason) in cases {
            let error =
                SectionTable::parse(&payload, &HEADER).expect_err("the section table is invalid");
            assert_eq!(corruption(error).1, expected_reason, "{payload:02x?}");
        }
    }

    #[test]
    fn parses_known_and_unknown_packets() -> anyhow::Result<()> {
        let decoded = encode_metadata(&[
            (PacketType::ARCHIVE_INFO, encode_archive_info(1)),
            (PacketType::FILE_INFO, encode_file_info(&standard_files())),
            (PacketType::TIMESTAMP_DICTIONARY, timestamp_dictionary()),
            (PacketType::RANGE_INDEX, vec![0x90]),
            (200, b"opaque".to_vec()),
            (201, Vec::new()),
        ]);
        let metadata = ArchiveMetadata::parse(HEADER, &decoded)?;
        assert_eq!(metadata.header(), &HEADER);
        assert_eq!(metadata.version().version, HEADER.version);
        assert!(metadata.sections().range(SectionId::SchemaTree).is_some());
        assert_eq!(metadata.timestamp_index().authoritative(), None);
        assert_eq!(
            metadata.range_index().map(|index| index.entries().len()),
            Some(0)
        );
        let unknown: Vec<_> = metadata
            .unknown_packets()
            .iter()
            .map(|packet| (packet.packet_type, packet.payload.to_vec()))
            .collect();
        assert_eq!(unknown, vec![(200, b"opaque".to_vec()), (201, Vec::new())]);
        Ok(())
    }

    #[test]
    fn zero_length_packets_count_as_absent() -> anyhow::Result<()> {
        let decoded = encode_metadata(&[
            (PacketType::ARCHIVE_INFO, Vec::new()),
            (PacketType::FILE_INFO, encode_file_info(&standard_files())),
            (PacketType::TIMESTAMP_DICTIONARY, Vec::new()),
            (PacketType::RANGE_INDEX, Vec::new()),
        ]);
        let metadata = ArchiveMetadata::parse(HEADER, &decoded)?;
        assert_eq!(metadata.timestamp_index().entries(), &[]);
        assert_eq!(
            metadata.range_index().map(|index| index.entries().len()),
            None
        );
        Ok(())
    }

    #[test]
    fn rejects_multi_segment_archives() {
        let decoded = encode_metadata(&[
            (PacketType::ARCHIVE_INFO, encode_archive_info(2)),
            (PacketType::FILE_INFO, encode_file_info(&standard_files())),
        ]);
        let error =
            ArchiveMetadata::parse(HEADER, &decoded).expect_err("two segments are unsupported");
        assert!(
            matches!(
                error,
                FormatError::UnsupportedFeature {
                    feature: UnsupportedFeature::NumSegments(2),
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn rejects_malformed_packet_framing() {
        let file_info = encode_file_info(&standard_files());
        let mut short = encode_metadata(&[(PacketType::FILE_INFO, file_info.clone())]).to_vec();
        short.truncate(short.len() - 1);
        let cases = [
            (
                SharedBytes::from(short),
                FormatLocation::MetadataPacket {
                    packet_type: PacketType::FILE_INFO,
                },
                CorruptionReason::ShortPayload,
            ),
            (
                encode_metadata(&[
                    (PacketType::FILE_INFO, file_info.clone()),
                    (PacketType::FILE_INFO, file_info),
                ]),
                FormatLocation::MetadataPacket {
                    packet_type: PacketType::FILE_INFO,
                },
                CorruptionReason::DuplicatePacket(PacketType::FILE_INFO),
            ),
            (
                encode_metadata(&[(PacketType::TIMESTAMP_DICTIONARY, timestamp_dictionary())]),
                FormatLocation::MetadataPacket {
                    packet_type: PacketType::FILE_INFO,
                },
                CorruptionReason::InvalidSectionTable,
            ),
            (
                encode_metadata(&[(PacketType::ARCHIVE_INFO, vec![0x80])]),
                FormatLocation::MetadataPacket {
                    packet_type: PacketType::ARCHIVE_INFO,
                },
                CorruptionReason::MalformedMsgpack,
            ),
        ];
        for (decoded, expected_location, expected_reason) in cases {
            let error =
                ArchiveMetadata::parse(HEADER, &decoded).expect_err("the metadata is invalid");
            assert_eq!(corruption(error), (expected_location, expected_reason));
        }
    }

    #[test]
    fn packets_are_parsed_from_exact_slices() -> anyhow::Result<()> {
        let mut timestamp_payload = 0_u64.to_le_bytes().to_vec();
        timestamp_payload.extend_from_slice(&1_u64.to_le_bytes());
        timestamp_payload.extend_from_slice(&7_u64.to_le_bytes());
        timestamp_payload.extend_from_slice(&2_u64.to_le_bytes());
        timestamp_payload.extend_from_slice(b"\\Y");

        let mut decoded = encode_metadata(&[
            (PacketType::FILE_INFO, encode_file_info(&standard_files())),
            (PacketType::TIMESTAMP_DICTIONARY, timestamp_payload.clone()),
        ])
        .to_vec();
        ArchiveMetadata::parse(HEADER, &SharedBytes::from(decoded.clone()))?;

        let size_position = decoded.len() - timestamp_payload.len() - 4;
        let short_size = u32::try_from(timestamp_payload.len() - 1)?;
        decoded[size_position..size_position + 4].copy_from_slice(&short_size.to_le_bytes());
        let error = ArchiveMetadata::parse(HEADER, &SharedBytes::from(decoded))
            .expect_err("the declared size cuts the last pattern");
        assert!(
            matches!(
                corruption(error),
                (
                    FormatLocation::MetadataPacket {
                        packet_type: PacketType::TIMESTAMP_DICTIONARY,
                    },
                    CorruptionReason::Truncated { .. },
                )
            ),
            "the payload must not be read past its declared size"
        );
        Ok(())
    }
}
