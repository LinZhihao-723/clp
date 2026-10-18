//! Fixture archives with one structure changed: the registered format divergences (DV-29, DV-30,
//! DV-32, DV-43) and the scope exclusions that must be rejected at open.

use clp_s_format::ArchiveIndex;
use clp_s_format::CorruptionReason;
use clp_s_format::FormatError;
use clp_s_format::FormatVersion;
use clp_s_format::OpenOptions;
use clp_s_format::SectionId;
use clp_s_format::TimestampPatternId;
use clp_s_format::UnsupportedFeature;
use clp_s_resources::ErrorKind;

use crate::support;
use crate::support::ArchiveParts;

const ARCHIVE_INFO_PACKET_TYPE: u8 = 0;
const RANGE_INDEX_PACKET_TYPE: u8 = 3;
const TIMESTAMP_DICTIONARY_PACKET_TYPE: u8 = 2;

/// # Returns
///
/// The parts of the fixture archive `file`.
fn fixture_parts(file: &str) -> ArchiveParts {
    ArchiveParts::from_archive(&support::read_bytes(
        &support::fixtures_dir().join("archives").join(file),
    ))
}

/// # Returns
///
/// The index of the archive assembled from `parts`, on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`support::open_index`]'s return values on failure.
fn open(parts: &ArchiveParts) -> Result<ArchiveIndex, FormatError> {
    support::open_index(&parts.encode(), OpenOptions::default())
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

/// A timestamp-dictionary payload split into its entries, its patterns and the bytes after them.
struct TimestampDictionaryParts {
    entries: Vec<u8>,
    patterns: Vec<(u64, Vec<u8>)>,
    trailing: Vec<u8>,
}

impl TimestampDictionaryParts {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The parts of the valid payload `payload`.
    fn split(payload: &[u8]) -> Self {
        let read_u64 = |position: &mut usize| {
            let value = u64::from_le_bytes(
                payload[*position..*position + 8]
                    .try_into()
                    .expect("eight bytes"),
            );
            *position += 8;
            value
        };
        let mut position = 0;
        for _ in 0..read_u64(&mut position) {
            let key_length = support::to_usize(read_u64(&mut position));
            position += key_length;
            let num_ids = support::to_usize(read_u64(&mut position));
            position += 4 * num_ids;
            if 1 == read_u64(&mut position) {
                position += 16;
            }
        }
        let entries = payload[..position].to_vec();
        let mut patterns = Vec::new();
        for _ in 0..read_u64(&mut position) {
            let id = read_u64(&mut position);
            let length = support::to_usize(read_u64(&mut position));
            patterns.push((id, payload[position..position + length].to_vec()));
            position += length;
        }
        Self {
            entries,
            patterns,
            trailing: payload[position..].to_vec(),
        }
    }

    /// # Returns
    ///
    /// The payload made of the parts.
    fn join(&self) -> Vec<u8> {
        let mut payload = self.entries.clone();
        payload.extend_from_slice(&(self.patterns.len() as u64).to_le_bytes());
        for (id, text) in &self.patterns {
            payload.extend_from_slice(&id.to_le_bytes());
            payload.extend_from_slice(&(text.len() as u64).to_le_bytes());
            payload.extend_from_slice(text);
        }
        payload.extend_from_slice(&self.trailing);
        payload
    }
}

#[test]
fn reassembling_a_fixture_preserves_its_index() -> anyhow::Result<()> {
    for path in support::readable_archives() {
        let bytes = support::read_bytes(&path);
        let original = support::open_index(&bytes, OpenOptions::default())?;
        let rebuilt = ArchiveParts::from_archive(&bytes).encode();
        let contents = support::read_everything(&rebuilt, OpenOptions::default(), 4096)?;
        assert_eq!(
            original.tables().tables(),
            contents.index.tables().tables(),
            "{}",
            path.display()
        );
    }
    Ok(())
}

#[test]
fn versions_outside_the_window_are_unsupported() {
    let base = fixture_parts("search.ts.clps");
    for (packed, is_accepted) in [
        (0x0005_0000_u32, true),
        (0x0005_0002, true),
        (0x0004_0000, false),
        (0x0004_0001, false),
        (0x0006_0000, false),
        (0x0105_0001, false),
    ] {
        let mut parts = base.clone();
        parts.header_prefix[4..8].copy_from_slice(&packed.to_le_bytes());
        match open(&parts) {
            Ok(index) => {
                assert!(is_accepted, "{packed:#x} must be rejected");
                assert_eq!(
                    index.metadata().version().version,
                    FormatVersion::from_packed(packed)
                );
                assert_eq!(
                    index.metadata().version().is_newer_patch,
                    packed == 0x0005_0002
                );
            }
            Err(error) => {
                assert!(!is_accepted, "{packed:#x} must be accepted: {error:?}");
                assert!(
                    matches!(
                        error,
                        FormatError::UnsupportedVersion(version) if version.to_packed() == packed
                    ),
                    "{error:?}"
                );
                assert_eq!(error.kind(), ErrorKind::UnsupportedArchive);
            }
        }
    }
}

#[test]
fn non_zstd_compression_is_unsupported() {
    let mut parts = fixture_parts("scalars.default.clps");
    parts.compression_type = 1;
    let error = open(&parts).expect_err("compression type 1 is unsupported");
    assert!(
        matches!(
            error,
            FormatError::UnsupportedFeature {
                feature: UnsupportedFeature::CompressionType(1),
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn multi_segment_archives_are_unsupported() {
    let mut parts = fixture_parts("scalars.default.clps");
    let mut archive_info = vec![0x81, 0xac];
    archive_info.extend_from_slice(b"num_segments");
    archive_info.push(0x02);
    *parts.packet_mut(ARCHIVE_INFO_PACKET_TYPE) = archive_info;
    let error = open(&parts).expect_err("two segments are unsupported");
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
fn trailing_timestamp_dictionary_bytes_are_ignored() -> anyhow::Result<()> {
    let base = fixture_parts("timestamps-int.ts.clps");
    let original = open(&base)?;
    let mut parts = base;
    parts
        .packet_mut(TIMESTAMP_DICTIONARY_PACKET_TYPE)
        .extend_from_slice(b"a future field");
    let index = open(&parts)?;
    let original_timestamps = original.metadata().timestamp_index();
    let timestamps = index.metadata().timestamp_index();
    assert_eq!(timestamps.entries(), original_timestamps.entries());
    assert_eq!(timestamps.pattern_ids(), original_timestamps.pattern_ids());
    Ok(())
}

#[test]
fn zero_length_and_unknown_packets_are_accepted() -> anyhow::Result<()> {
    let mut parts = fixture_parts("timestamps-int.ts.clps");
    *parts.packet_mut(RANGE_INDEX_PACKET_TYPE) = Vec::new();
    parts.packets.push((77, Vec::new()));
    parts.packets.push((78, b"opaque".to_vec()));
    let index = open(&parts)?;
    assert_eq!(
        index
            .metadata()
            .range_index()
            .map(|range_index| range_index.entries().len()),
        None
    );
    let unknown: Vec<_> = index
        .metadata()
        .unknown_packets()
        .iter()
        .map(|packet| (packet.packet_type, packet.payload.to_vec()))
        .collect();
    assert_eq!(unknown, vec![(77, Vec::new()), (78, b"opaque".to_vec())]);
    Ok(())
}

#[test]
fn an_archive_without_tables_is_readable() -> anyhow::Result<()> {
    let mut parts = fixture_parts("scalars.default.clps");
    parts.set_decoded_section(SectionId::TableMetadata, &[0; 24]);
    parts.raw_section_mut(SectionId::PackedStreams).clear();
    let contents = support::read_everything(&parts.encode(), OpenOptions::default(), 4096)?;
    assert_eq!(contents.index.tables().num_log_events(), 0);
    assert_eq!(
        contents.decoded_streams,
        Vec::<clp_s_format::SharedBytes>::new()
    );
    Ok(())
}

#[test]
fn an_unparsable_timestamp_pattern_fails_only_on_access() -> anyhow::Result<()> {
    let mut parts = fixture_parts("timestamps-int.ts.clps");
    let payload = parts.packet_mut(TIMESTAMP_DICTIONARY_PACKET_TYPE);
    let mut dictionary = TimestampDictionaryParts::split(payload);
    assert!(
        dictionary.patterns.len() > 1,
        "the fixture has several patterns"
    );
    dictionary.patterns[0].1 = vec![0xff, 0xfe];
    *payload = dictionary.join();
    let patterns = dictionary.patterns;

    let index = open(&parts)?;
    let timestamps = index.metadata().timestamp_index();
    let broken_id = TimestampPatternId::new(patterns[0].0);
    let error = timestamps
        .pattern(broken_id)
        .expect_err("the broken pattern cannot compile");
    assert!(
        matches!(
            error,
            FormatError::UnsupportedFeature {
                feature: UnsupportedFeature::TimestampPattern(id),
                ..
            } if id == broken_id.get()
        ),
        "{error:?}"
    );
    for (id, _) in &patterns[1..] {
        timestamps.pattern(TimestampPatternId::new(*id))?;
    }
    Ok(())
}

#[test]
fn a_duplicate_schema_tree_triple_is_corrupt() {
    let mut parts = fixture_parts("scalars.default.clps");
    let mut tree = parts.decoded_section(SectionId::SchemaTree);
    let num_nodes = u64::from_le_bytes(tree[..8].try_into().expect("eight bytes"));
    tree[..8].copy_from_slice(&(num_nodes + 1).to_le_bytes());
    let first_node_size = 4
        + 8
        + support::to_usize(u64::from_le_bytes(
            tree[12..20].try_into().expect("eight bytes"),
        ))
        + 1;
    let first_node = tree[8..8 + first_node_size].to_vec();
    tree.extend_from_slice(&first_node);
    parts.set_decoded_section(SectionId::SchemaTree, &tree);

    let error = open(&parts).expect_err("a duplicate triple shifts C++ node ids");
    assert!(
        matches!(
            corruption_reason(error),
            CorruptionReason::DuplicateNode { node } if u64::from(node) == num_nodes
        ),
        "the duplicate is the appended node"
    );
}

#[test]
fn unknown_node_types_depend_on_the_patch_release() {
    let mut parts = fixture_parts("scalars.default.clps");
    let mut tree = parts.decoded_section(SectionId::SchemaTree);
    let num_nodes = u64::from_le_bytes(tree[..8].try_into().expect("eight bytes"));
    tree[..8].copy_from_slice(&(num_nodes + 1).to_le_bytes());
    tree.extend_from_slice(&(-1_i32).to_le_bytes());
    tree.extend_from_slice(&1_u64.to_le_bytes());
    tree.extend_from_slice(b"x");
    tree.push(200);
    parts.set_decoded_section(SectionId::SchemaTree, &tree);

    let error = open(&parts).expect_err("type 200 is unknown to 0.5.1");
    assert_eq!(
        corruption_reason(error),
        CorruptionReason::UnknownNodeType(200)
    );

    parts.header_prefix[4..8].copy_from_slice(&0x0005_0002_u32.to_le_bytes());
    let error = open(&parts).expect_err("type 200 is unknown to this reader");
    assert!(
        matches!(
            error,
            FormatError::UnsupportedFeature {
                feature: UnsupportedFeature::NewerNodeType(200),
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_schema_with_an_unordered_region_is_unsupported() {
    let mut parts = fixture_parts("scalars.default.clps");
    let mut schema_map = parts.decoded_section(SectionId::SchemaMap);
    let num_nodes = u32::from_le_bytes(schema_map[12..16].try_into().expect("four bytes"));
    assert!(num_nodes > 0, "the first schema has nodes");
    schema_map[16..20].copy_from_slice(&(num_nodes - 1).to_le_bytes());
    parts.set_decoded_section(SectionId::SchemaMap, &schema_map);

    let error = open(&parts).expect_err("an unordered region means structured arrays");
    assert!(
        matches!(
            error,
            FormatError::UnsupportedFeature {
                feature: UnsupportedFeature::StructuredArrays,
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(error.kind(), ErrorKind::UnsupportedArchive);
}

#[test]
fn reader_limits_are_resource_errors() {
    let bytes = support::read_bytes(&support::fixtures_dir().join("archives/nested.default.clps"));
    let limited = OpenOptions {
        limits: clp_s_format::ReaderLimits {
            max_schema_tree_depth: 3,
            ..clp_s_format::ReaderLimits::default()
        },
        ..OpenOptions::default()
    };
    let error = support::open_index(&bytes, limited).expect_err("the fixture nests deeper than 3");
    assert!(
        matches!(error, FormatError::LimitExceeded { .. }),
        "{error:?}"
    );
    assert_eq!(error.kind(), ErrorKind::ResourceExhausted);

    let limited = OpenOptions {
        limits: clp_s_format::ReaderLimits {
            max_metadata_section_size: 16,
            ..clp_s_format::ReaderLimits::default()
        },
        ..OpenOptions::default()
    };
    let error = support::open_index(&bytes, limited).expect_err("the metadata exceeds 16 bytes");
    assert!(
        matches!(error, FormatError::LimitExceeded { .. }),
        "{error:?}"
    );
}
