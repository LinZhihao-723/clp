//! The sans-IO, staged archive opener.

use std::sync::Arc;

use crate::ArchiveHeader;
use crate::ArchiveIndex;
use crate::ArchiveMetadata;
use crate::ByteRange;
use crate::CorruptionReason;
use crate::FormatError;
use crate::FormatLocation;
use crate::OpenOptions;
use crate::SchemaMap;
use crate::SchemaTree;
use crate::SectionId;
use crate::SharedBytes;
use crate::StreamDecoder;
use crate::TableDirectory;
use crate::metadata::PacketType;

/// What the opener needs next, or which checkpoint it reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenStep {
    /// Fetch these absolute ranges and pass each to [`ArchiveOpener::push_range`].
    NeedsData(Vec<ByteRange>),

    /// The header and metadata are decoded; [`ArchiveOpener::metadata`] is available.
    MetadataReady,

    /// The schema layer is decoded; finish with [`ArchiveOpener::into_index`].
    IndexReady,
}

/// I/O-free, staged archive opener; drivers feed it the bytes it asks for.
///
/// The steps are: the prefix `[0, prefix_fetch_size)` (header, and usually the whole metadata
/// section and schema layer), any missing part of the metadata section,
/// [`OpenStep::MetadataReady`], the schema layer (`/schema_tree`, `/schema_ids`, `/table_metadata`,
/// which are contiguous), and [`OpenStep::IndexReady`].
#[derive(Debug)]
pub struct ArchiveOpener {
    options: OpenOptions,
    file_size: u64,
    stage: Stage,
    pushed_ranges: Vec<(ByteRange, SharedBytes)>,
    decoder: Option<StreamDecoder>,
    metadata: Option<Arc<ArchiveMetadata>>,
    index: Option<ArchiveIndex>,
}

impl ArchiveOpener {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// An opener for an archive file of `file_size` bytes.
    #[must_use]
    pub const fn new(options: OpenOptions, file_size: u64) -> Self {
        Self {
            options,
            file_size,
            stage: Stage::Start,
            pushed_ranges: Vec::new(),
            decoder: None,
            metadata: None,
            index: None,
        }
    }

    /// Advances to the next request or checkpoint.
    ///
    /// # Returns
    ///
    /// The next step on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if a structure fails validation.
    /// * [`FormatError::UnsupportedVersion`] if the version is outside [0.5.0, 0.5.x].
    /// * [`FormatError::UnsupportedFeature`] if the archive uses an unsupported construct (for
    ///   example structured arrays).
    /// * [`FormatError::LimitExceeded`] if a size or count exceeds [`crate::ReaderLimits`].
    pub fn advance(&mut self) -> Result<OpenStep, FormatError> {
        loop {
            match self.stage {
                Stage::Start => {
                    if self.file_size < ArchiveHeader::SIZE {
                        return Err(FormatError::corrupt(
                            FormatLocation::Header,
                            CorruptionReason::Truncated {
                                needed_size: ArchiveHeader::SIZE,
                                available_size: self.file_size,
                            },
                        ));
                    }
                    let prefix_size = self
                        .options
                        .prefix_fetch_size
                        .max(ArchiveHeader::SIZE)
                        .min(self.file_size);
                    self.stage = Stage::Header(ByteRange {
                        begin: 0,
                        end: prefix_size,
                    });
                }
                Stage::Header(prefix) => {
                    let Some(prefix_bytes) = self.find_bytes(prefix) else {
                        return Ok(OpenStep::NeedsData(vec![prefix]));
                    };
                    let header = ArchiveHeader::parse(&prefix_bytes, self.file_size)?;
                    FormatError::check_limit(
                        "metadata section size",
                        header.metadata_section_size.into(),
                        self.options.limits.max_metadata_section_size,
                    )?;
                    self.stage = Stage::Metadata(header);
                }
                Stage::Metadata(header) => {
                    let range = ByteRange {
                        begin: ArchiveHeader::SIZE,
                        end: header.sections_offset(),
                    };
                    let Some(frame) = self.find_bytes(range) else {
                        return Ok(OpenStep::NeedsData(vec![range]));
                    };
                    let limits = self.options.limits;
                    let decoded = self.decoder()?.decode_section(
                        &frame,
                        FormatLocation::MetadataSection,
                        &limits,
                    )?;
                    self.metadata = Some(Arc::new(ArchiveMetadata::parse(header, &decoded)?));
                    self.stage = Stage::MetadataReady;
                    return Ok(OpenStep::MetadataReady);
                }
                Stage::MetadataReady => {
                    let range = self.schema_layer_range()?;
                    self.stage = Stage::SchemaLayer(range);
                }
                Stage::SchemaLayer(range) => {
                    if self.find_bytes(range).is_none() {
                        return Ok(OpenStep::NeedsData(vec![range]));
                    }
                    self.index = Some(self.parse_schema_layer()?);
                    self.pushed_ranges.clear();
                    self.stage = Stage::IndexReady;
                    return Ok(OpenStep::IndexReady);
                }
                Stage::IndexReady => return Ok(OpenStep::IndexReady),
            }
        }
    }

    /// Supplies the bytes of a range requested through [`OpenStep::NeedsData`].
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if `bytes` is shorter than `range`.
    pub fn push_range(&mut self, range: ByteRange, bytes: SharedBytes) -> Result<(), FormatError> {
        let available_size = bytes.len() as u64;
        if available_size < range.len() {
            return Err(FormatError::corrupt(
                FormatLocation::Header,
                CorruptionReason::Truncated {
                    needed_size: range.len(),
                    available_size,
                },
            ));
        }
        let mut bytes = bytes;
        bytes.truncate(usize::try_from(range.len()).unwrap_or(usize::MAX));
        self.pushed_ranges.push((range, bytes));
        Ok(())
    }

    /// # Returns
    ///
    /// The decoded metadata once [`OpenStep::MetadataReady`] has been reached, `None` before.
    #[must_use]
    pub fn metadata(&self) -> Option<&ArchiveMetadata> {
        self.metadata.as_deref()
    }

    /// Finishes the open after [`OpenStep::IndexReady`].
    ///
    /// # Returns
    ///
    /// The archive index on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if called before [`OpenStep::IndexReady`] was reached (a driver
    ///   bug, reported rather than panicking).
    pub fn into_index(self) -> Result<ArchiveIndex, FormatError> {
        self.index.ok_or_else(|| {
            FormatError::corrupt(FormatLocation::Header, CorruptionReason::OpenerNotReady)
        })
    }

    /// # Returns
    ///
    /// The bytes of `range` if a pushed range covers it, without copying.
    fn find_bytes(&self, range: ByteRange) -> Option<SharedBytes> {
        self.pushed_ranges
            .iter()
            .find(|(pushed, _)| pushed.begin <= range.begin && range.end <= pushed.end)
            .and_then(|(pushed, bytes)| {
                let begin = usize::try_from(range.begin - pushed.begin).ok()?;
                let end = usize::try_from(range.end - pushed.begin).ok()?;
                Some(bytes.slice(begin..end))
            })
    }

    /// # Returns
    ///
    /// The decoder, created on first use, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`StreamDecoder::new`]'s return values on failure.
    fn decoder(&mut self) -> Result<&mut StreamDecoder, FormatError> {
        let decoder = match self.decoder.take() {
            Some(decoder) => decoder,
            None => StreamDecoder::new()?,
        };
        Ok(self.decoder.insert(decoder))
    }

    /// # Returns
    ///
    /// The metadata decoded so far, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if the metadata checkpoint has not been reached.
    fn decoded_metadata(&self) -> Result<Arc<ArchiveMetadata>, FormatError> {
        self.metadata.clone().ok_or_else(|| {
            FormatError::corrupt(FormatLocation::Header, CorruptionReason::OpenerNotReady)
        })
    }

    /// # Returns
    ///
    /// The range covering the three schema-layer sections on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::LimitExceeded`] if a section is larger than
    ///   [`crate::ReaderLimits::max_section_decoded_size`].
    /// * Forwards [`Self::decoded_metadata`]'s return values on failure.
    /// * Forwards [`Self::section_range`]'s return values on failure.
    fn schema_layer_range(&self) -> Result<ByteRange, FormatError> {
        let metadata = self.decoded_metadata()?;
        let mut layer: Option<ByteRange> = None;
        for section in SCHEMA_LAYER {
            let range = Self::section_range(&metadata, section)?;
            FormatError::check_limit(
                "compressed schema-layer section size",
                range.len(),
                self.options.limits.max_section_decoded_size,
            )?;
            layer = Some(layer.map_or(range, |layer| ByteRange {
                begin: layer.begin.min(range.begin),
                end: layer.end.max(range.end),
            }));
        }
        Ok(layer.unwrap_or(ByteRange { begin: 0, end: 0 }))
    }

    /// Decodes and validates the schema tree, schema map and table directory.
    ///
    /// # Returns
    ///
    /// The archive index on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if a section's bytes were not pushed.
    /// * Forwards [`Self::decoded_metadata`]'s return values on failure.
    /// * Forwards [`Self::section_range`]'s return values on failure.
    /// * Forwards [`StreamDecoder::decode_section`]'s return values on failure.
    /// * Forwards [`SchemaTree::parse`]'s return values on failure.
    /// * Forwards [`SchemaMap::parse`]'s return values on failure.
    /// * Forwards [`TableDirectory::parse`]'s return values on failure.
    fn parse_schema_layer(&mut self) -> Result<ArchiveIndex, FormatError> {
        let metadata = self.decoded_metadata()?;
        let limits = self.options.limits;
        let mut decoded_sections = Vec::with_capacity(SCHEMA_LAYER.len());
        for section in SCHEMA_LAYER {
            let range = Self::section_range(&metadata, section)?;
            let frame = self.find_bytes(range).ok_or_else(|| {
                FormatError::corrupt(
                    FormatLocation::Section(section),
                    CorruptionReason::OpenerNotReady,
                )
            })?;
            let location = FormatLocation::Section(section);
            decoded_sections.push(self.decoder()?.decode_section(&frame, location, &limits)?);
        }
        let [tree_section, map_section, table_section] =
            <[SharedBytes; 3]>::try_from(decoded_sections).map_err(|_| {
                FormatError::corrupt(FormatLocation::Header, CorruptionReason::OpenerNotReady)
            })?;

        let schema_tree = SchemaTree::parse(tree_section, &limits, metadata.version())?;
        let schema_map = SchemaMap::parse(&map_section, &schema_tree)?;
        let packed_streams = Self::section_range(&metadata, SectionId::PackedStreams)?;
        let tables = TableDirectory::parse(&table_section, packed_streams, &schema_map)?;
        Ok(ArchiveIndex::new(metadata, schema_tree, schema_map, tables))
    }

    /// # Returns
    ///
    /// The range of `section` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::Corrupt`] if the archive has no such section.
    fn section_range(
        metadata: &ArchiveMetadata,
        section: SectionId,
    ) -> Result<ByteRange, FormatError> {
        metadata.sections().range(section).ok_or_else(|| {
            FormatError::corrupt(
                FormatLocation::MetadataPacket {
                    packet_type: PacketType::FILE_INFO,
                },
                CorruptionReason::MissingSection(section),
            )
        })
    }
}

const SCHEMA_LAYER: [SectionId; 3] = [
    SectionId::SchemaTree,
    SectionId::SchemaMap,
    SectionId::TableMetadata,
];

/// Where the opener is in the protocol.
#[derive(Clone, Copy, Debug)]
enum Stage {
    Start,
    Header(ByteRange),
    Metadata(ArchiveHeader),
    MetadataReady,
    SchemaLayer(ByteRange),
    IndexReady,
}

#[cfg(test)]
mod tests {
    use super::ArchiveOpener;
    use super::OpenStep;
    use crate::ArchiveHeader;
    use crate::ByteRange;
    use crate::CorruptionReason;
    use crate::FormatError;
    use crate::OpenOptions;
    use crate::SectionId;
    use crate::SharedBytes;

    const ARCHIVE: &[u8] = include_bytes!("../../../tests/fixtures/archives/search.ts.clps");

    /// Pushes the fixture bytes of `range`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`ArchiveOpener::push_range`]'s return values on failure.
    fn push(opener: &mut ArchiveOpener, range: ByteRange) -> Result<(), FormatError> {
        let begin = usize::try_from(range.begin).expect("small archive");
        let end = usize::try_from(range.end).expect("small archive");
        opener.push_range(range, SharedBytes::from_static(&ARCHIVE[begin..end]))
    }

    /// # Returns
    ///
    /// The corruption reason of `error`.
    fn corruption_reason(error: FormatError) -> CorruptionReason {
        match error {
            FormatError::Corrupt { reason, .. } => *reason,
            other => panic!("expected a corruption error, got {other:?}"),
        }
    }

    #[test]
    fn a_large_prefix_needs_one_read() -> anyhow::Result<()> {
        let mut opener = ArchiveOpener::new(OpenOptions::default(), ARCHIVE.len() as u64);
        let whole_file = ByteRange {
            begin: 0,
            end: ARCHIVE.len() as u64,
        };
        assert_eq!(opener.advance()?, OpenStep::NeedsData(vec![whole_file]));
        assert!(opener.metadata().is_none(), "{:?}", opener.metadata());
        push(&mut opener, whole_file)?;
        assert_eq!(opener.advance()?, OpenStep::MetadataReady);
        assert!(opener.metadata().is_some());
        assert_eq!(opener.advance()?, OpenStep::IndexReady);
        assert_eq!(opener.advance()?, OpenStep::IndexReady);
        let index = opener.into_index()?;
        assert_eq!(index.tables().num_log_events(), 40);
        Ok(())
    }

    #[test]
    fn a_header_sized_prefix_fetches_each_stage() -> anyhow::Result<()> {
        let options = OpenOptions {
            prefix_fetch_size: 0,
            ..OpenOptions::default()
        };
        let mut opener = ArchiveOpener::new(options, ARCHIVE.len() as u64);
        let header = ByteRange {
            begin: 0,
            end: ArchiveHeader::SIZE,
        };
        assert_eq!(opener.advance()?, OpenStep::NeedsData(vec![header]));
        assert_eq!(
            opener.advance()?,
            OpenStep::NeedsData(vec![header]),
            "the opener asks again until the bytes arrive"
        );
        push(&mut opener, header)?;

        let OpenStep::NeedsData(ranges) = opener.advance()? else {
            panic!("the metadata section is not in the prefix");
        };
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].begin, ArchiveHeader::SIZE);
        push(&mut opener, ranges[0])?;
        assert_eq!(opener.advance()?, OpenStep::MetadataReady);

        let metadata = opener.metadata().expect("the metadata is decoded");
        let tree = metadata
            .sections()
            .range(SectionId::SchemaTree)
            .expect("the tree exists");
        let table_metadata = metadata
            .sections()
            .range(SectionId::TableMetadata)
            .expect("the table metadata exists");
        let schema_layer = ByteRange {
            begin: tree.begin,
            end: table_metadata.end,
        };
        assert_eq!(opener.advance()?, OpenStep::NeedsData(vec![schema_layer]));
        push(&mut opener, schema_layer)?;
        assert_eq!(opener.advance()?, OpenStep::IndexReady);
        assert_eq!(opener.into_index()?.tables().tables().len(), 34);
        Ok(())
    }

    #[test]
    fn misuse_and_short_inputs_are_errors() {
        let opener = ArchiveOpener::new(OpenOptions::default(), ARCHIVE.len() as u64);
        let error = opener.into_index().expect_err("the index is not ready");
        assert_eq!(corruption_reason(error), CorruptionReason::OpenerNotReady);

        let mut opener = ArchiveOpener::new(OpenOptions::default(), ARCHIVE.len() as u64);
        let error = opener
            .push_range(
                ByteRange { begin: 0, end: 64 },
                SharedBytes::from_static(&ARCHIVE[..63]),
            )
            .expect_err("63 bytes cannot fill a 64-byte range");
        assert_eq!(
            corruption_reason(error),
            CorruptionReason::Truncated {
                needed_size: 64,
                available_size: 63,
            }
        );

        let mut opener = ArchiveOpener::new(OpenOptions::default(), 63);
        let error = opener.advance().expect_err("63 bytes cannot hold a header");
        assert_eq!(
            corruption_reason(error),
            CorruptionReason::Truncated {
                needed_size: 64,
                available_size: 63,
            }
        );
    }
}
