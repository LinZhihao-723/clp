//! Mutation tests: corrupted fixture archives (byte flips, truncations, rewritten size and count
//! fields) must be read to a typed error or to a consistent result, and never panic.
//!
//! Every integer field of a decoded structure (sizes, counts, offsets, ids, and msgpack headers) is
//! located by walking the structure and mutated exhaustively; the bytes between fields (keys,
//! dictionary entries, strings) are sampled with a stride.

use std::panic::AssertUnwindSafe;

use clp_s_format::LogTypeDictionary;
use clp_s_format::OpenOptions;
use clp_s_format::SectionId;
use clp_s_format::SharedBytes;
use clp_s_format::VariableDictionary;

use crate::support;
use crate::support::ArchiveParts;

/// Fixtures with every kind of structure: timestamps and patterns, a range index, arrays, kv-IR
/// input, log order off, and multi-table, multi-stream directories.
const STRUCTURED_FIXTURES: [&str; 6] = [
    "arrays.default.clps",
    "kvir-text.ts.clps",
    "many-schemas.tables256.clps",
    "scalars.nolog.clps",
    "search.ts.clps",
    "timestamps-int.ts.clps",
];

/// The most positions mutated per structure; larger structures are sampled with a stride.
const MAX_POSITIONS: usize = 512;

/// The metadata packet type of the timestamp dictionary; the other known packets are msgpack.
const TIMESTAMP_DICTIONARY_PACKET_TYPE: u8 = 2;

/// The largest known metadata packet type.
const MAX_KNOWN_PACKET_TYPE: u8 = 3;

/// The outcome counts of a mutation run.
#[derive(Debug, Default)]
struct Outcomes {
    num_errors: usize,
    num_successes: usize,
    panics: Vec<String>,
}

impl Outcomes {
    /// Reads `archive` in full, recording whether it fails, succeeds or panics.
    ///
    /// # Returns
    ///
    /// Whether the read returned an error.
    fn read(&mut self, archive: &SharedBytes, description: &str) -> bool {
        self.run(description, || {
            support::read_everything(archive, OpenOptions::default(), 4096).map(|_| ())
        })
    }

    /// Runs `operation`, recording whether it fails, succeeds or panics.
    ///
    /// # Returns
    ///
    /// Whether the operation returned an error.
    fn run(&mut self, description: &str, operation: impl FnOnce() -> anyhow::Result<()>) -> bool {
        match std::panic::catch_unwind(AssertUnwindSafe(operation)) {
            Ok(Ok(())) => {
                self.num_successes += 1;
                false
            }
            Ok(Err(_)) => {
                self.num_errors += 1;
                true
            }
            Err(_) => {
                self.panics.push(description.to_owned());
                false
            }
        }
    }

    /// Asserts that no read panicked and that at least one read failed.
    fn assert_no_panics(&self) {
        assert_eq!(self.panics, Vec::<String>::new(), "reads panicked");
        assert!(self.num_errors > 0, "mutations must be detected: {self:?}");
    }
}

/// An integer field of a structure.
#[derive(Clone, Copy, Debug)]
struct Field {
    offset: usize,
    width: usize,
    is_big_endian: bool,
}

impl Field {
    /// # Returns
    ///
    /// The field's value in `structure`.
    fn read(self, structure: &[u8]) -> u64 {
        let bytes = &structure[self.offset..self.offset + self.width];
        let fold = |value: u64, byte: &u8| (value << 8) | u64::from(*byte);
        if self.is_big_endian {
            bytes.iter().fold(0, fold)
        } else {
            bytes.iter().rev().fold(0, fold)
        }
    }

    /// Writes the low [`Self::width`] bytes of `value` over the field.
    fn write(self, structure: &mut [u8], value: u64) {
        let bytes = &mut structure[self.offset..self.offset + self.width];
        for (index, byte) in bytes.iter_mut().enumerate() {
            let shift = if self.is_big_endian {
                8 * (self.width - 1 - index)
            } else {
                8 * index
            };
            *byte = (value >> shift).to_le_bytes()[0];
        }
    }

    /// # Returns
    ///
    /// The values to write over the field: the boundary values of its width and the neighbours of
    /// its current value in `structure`, without the current value itself.
    fn overwrites(self, structure: &[u8]) -> Vec<u64> {
        let num_bits = 8 * u32::try_from(self.width).expect("a field is at most 8 bytes");
        let max = u64::MAX >> (64 - num_bits);
        let current = self.read(structure);
        let mut values = vec![
            0,
            1,
            max,
            1 << (num_bits - 1),
            (1 << (num_bits - 1)) - 1,
            current.wrapping_add(1) & max,
            current.wrapping_sub(1) & max,
            current ^ (1 << (num_bits - 1)),
        ];
        if 8 == self.width {
            values.extend([1 << 40, u64::from(u32::MAX) + 1]);
        }
        values.sort_unstable();
        values.dedup();
        values.retain(|value| *value != current);
        values
    }
}

/// Walks a valid structure, recording the position of every integer field it reads.
struct FieldLocator<'structure> {
    structure: &'structure [u8],
    position: usize,
    fields: Vec<Field>,
}

impl<'structure> FieldLocator<'structure> {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A locator at the start of `structure`.
    const fn new(structure: &'structure [u8]) -> Self {
        Self {
            structure,
            position: 0,
            fields: Vec::new(),
        }
    }

    /// Records a little-endian field of `width` bytes at the current position and moves past it.
    ///
    /// # Returns
    ///
    /// The field's value.
    fn field(&mut self, width: usize) -> u64 {
        self.record(width, false)
    }

    /// Records a big-endian (msgpack) field of `width` bytes at the current position and moves
    /// past it.
    ///
    /// # Returns
    ///
    /// The field's value.
    fn big_endian_field(&mut self, width: usize) -> u64 {
        self.record(width, true)
    }

    /// Moves past `size` bytes that hold no field.
    fn skip(&mut self, size: u64) {
        self.position += support::to_usize(size);
        assert!(
            self.position <= self.structure.len(),
            "the source structure is valid"
        );
    }

    /// Records every header, length and integer of the msgpack value at the current position.
    fn msgpack_value(&mut self) {
        let mut num_pending_values: u64 = 1;
        while num_pending_values > 0 {
            num_pending_values -= 1;
            let header = u8::try_from(self.field(1)).expect("a one-byte field");
            match header {
                0x80..=0x8f => num_pending_values += 2 * u64::from(header & 0x0f),
                0x90..=0x9f => num_pending_values += u64::from(header & 0x0f),
                0xa0..=0xbf => self.skip(u64::from(header & 0x1f)),
                0xc4 | 0xd9 => {
                    let size = self.big_endian_field(1);
                    self.skip(size);
                }
                0xc5 | 0xda => {
                    let size = self.big_endian_field(2);
                    self.skip(size);
                }
                0xc6 | 0xdb => {
                    let size = self.big_endian_field(4);
                    self.skip(size);
                }
                0xc7..=0xc9 => {
                    let size = self.big_endian_field(1 << (header - 0xc7));
                    self.skip(1 + size);
                }
                0xca => self.skip(4),
                0xcb => self.skip(8),
                0xcc..=0xcf => {
                    self.big_endian_field(1 << (header - 0xcc));
                }
                0xd0..=0xd3 => {
                    self.big_endian_field(1 << (header - 0xd0));
                }
                0xd4..=0xd8 => self.skip(1 + (1 << (header - 0xd4))),
                0xdc => num_pending_values += self.big_endian_field(2),
                0xdd => num_pending_values += self.big_endian_field(4),
                0xde => num_pending_values += 2 * self.big_endian_field(2),
                0xdf => num_pending_values += 2 * self.big_endian_field(4),
                _ => {}
            }
        }
    }

    /// # Returns
    ///
    /// The located fields, asserting that the walk consumed the whole structure.
    fn finish(self) -> Vec<Field> {
        assert_eq!(
            self.position,
            self.structure.len(),
            "the field walk must consume the structure"
        );
        self.fields
    }

    /// Records a field of `width` bytes at the current position and moves past it.
    ///
    /// # Returns
    ///
    /// The field's value.
    fn record(&mut self, width: usize, is_big_endian: bool) -> u64 {
        let field = Field {
            offset: self.position,
            width,
            is_big_endian,
        };
        self.skip(width as u64);
        self.fields.push(field);
        field.read(self.structure)
    }
}

/// # Returns
///
/// Every field of a decoded metadata section: the packet count, each packet's type and size, and
/// the fields of the timestamp dictionary and of the msgpack packets.
fn metadata_fields(decoded: &[u8]) -> Vec<Field> {
    let mut locator = FieldLocator::new(decoded);
    let num_packets = locator.field(1);
    for _ in 0..num_packets {
        let packet_type = u8::try_from(locator.field(1)).expect("a one-byte field");
        let size = locator.field(4);
        let end = locator.position + support::to_usize(size);
        if 0 == size || packet_type > MAX_KNOWN_PACKET_TYPE {
            locator.skip(size);
        } else if TIMESTAMP_DICTIONARY_PACKET_TYPE == packet_type {
            locate_timestamp_dictionary(&mut locator);
        } else {
            locator.msgpack_value();
        }
        assert_eq!(
            locator.position, end,
            "packet {packet_type} is walked exactly"
        );
    }
    locator.finish()
}

/// Locates the fields of a timestamp-dictionary packet at the current position.
fn locate_timestamp_dictionary(locator: &mut FieldLocator<'_>) {
    const EPOCH_ENCODING: u64 = 1;

    let num_entries = locator.field(8);
    for _ in 0..num_entries {
        let key_size = locator.field(8);
        locator.skip(key_size);
        locator.field(8);
        locator.field(4);
        if EPOCH_ENCODING == locator.field(8) {
            locator.field(8);
            locator.field(8);
        }
    }
    let num_patterns = locator.field(8);
    for _ in 0..num_patterns {
        locator.field(8);
        let pattern_size = locator.field(8);
        locator.skip(pattern_size);
    }
}

/// # Returns
///
/// Every field of the decoded payload of `section`, or of a dictionary's entries given its
/// `num_dictionary_entries`.
fn section_fields(section: SectionId, decoded: &[u8], num_dictionary_entries: u64) -> Vec<Field> {
    let mut locator = FieldLocator::new(decoded);
    match section {
        SectionId::SchemaTree => {
            let num_nodes = locator.field(8);
            for _ in 0..num_nodes {
                locator.field(4);
                let key_size = locator.field(8);
                locator.skip(key_size);
                locator.field(1);
            }
        }
        SectionId::SchemaMap => {
            let num_schemas = locator.field(8);
            for _ in 0..num_schemas {
                locator.field(4);
                let num_nodes = locator.field(4);
                locator.field(4);
                for _ in 0..num_nodes {
                    locator.field(4);
                }
            }
        }
        SectionId::TableMetadata => {
            let num_streams = locator.field(8);
            for _ in 0..num_streams {
                locator.field(8);
                locator.field(8);
            }
            locator.field(8);
            let num_tables = locator.field(8);
            for _ in 0..num_tables {
                locator.field(8);
                locator.field(8);
                locator.field(4);
                locator.field(8);
            }
        }
        SectionId::VariableDictionary
        | SectionId::LogTypeDictionary
        | SectionId::ArrayDictionary => {
            for _ in 0..num_dictionary_entries {
                let entry_size = locator.field(8);
                locator.skip(entry_size);
            }
        }
        _ => panic!("section {} has no located fields", section.name()),
    }
    locator.finish()
}

/// # Returns
///
/// The positions to mutate in a structure of `size` bytes.
fn positions(size: usize) -> impl Iterator<Item = usize> {
    (0..size).step_by(size.div_ceil(MAX_POSITIONS).max(1))
}

/// # Returns
///
/// The byte mutations applied at one position.
const fn byte_mutations(byte: u8) -> [u8; 3] {
    [byte ^ 0x01, byte ^ 0x80, 0xff]
}

/// Overwrites every field of `structure` with each of its [`Field::overwrites`] and truncates the
/// structure at every field, then applies the byte mutations, an 8-byte size-field overwrite and a
/// truncation at the sampled [`positions`], reading the archive `rebuild` makes of each mutated
/// copy.
///
/// # Returns
///
/// The outcomes; if `must_truncations_fail`, every truncation must have failed.
fn mutate_structure(
    structure: &[u8],
    fields: &[Field],
    description: &str,
    must_truncations_fail: bool,
    rebuild: &dyn Fn(&[u8]) -> SharedBytes,
) -> Outcomes {
    assert!(!fields.is_empty(), "{description}: no field was located");
    let mut outcomes = Outcomes::default();
    for field in fields {
        for value in field.overwrites(structure) {
            let mut mutated = structure.to_vec();
            field.write(&mut mutated, value);
            outcomes.read(
                &rebuild(&mutated),
                &format!(
                    "{description}: {}-byte field at {} = {value:#x}",
                    field.width, field.offset
                ),
            );
        }
    }
    let field_offsets = fields.iter().map(|field| field.offset);
    for position in positions(structure.len()) {
        for byte in byte_mutations(structure[position]) {
            let mut mutated = structure.to_vec();
            mutated[position] = byte;
            outcomes.read(
                &rebuild(&mutated),
                &format!("{description}: byte {position} = {byte:#04x}"),
            );
        }
        for value in [u64::MAX, 1 << 40, u64::from(u32::MAX) + 1, 0] {
            let mut mutated = structure.to_vec();
            let end = (position + 8).min(mutated.len());
            mutated[position..end].copy_from_slice(&value.to_le_bytes()[..end - position]);
            outcomes.read(
                &rebuild(&mutated),
                &format!("{description}: u64 at {position} = {value:#x}"),
            );
        }
    }
    for length in field_offsets.chain(positions(structure.len())) {
        let is_error = outcomes.read(
            &rebuild(&structure[..length]),
            &format!("{description}: truncated to {length}"),
        );
        assert!(
            is_error || !must_truncations_fail || !outcomes.panics.is_empty(),
            "{description}: truncation to {length} of {} bytes must fail",
            structure.len()
        );
    }
    outcomes
}

/// Runs [`mutate_structure`] over one decoded section of every structured fixture.
fn mutate_section_of_every_fixture(section: SectionId) {
    for file in STRUCTURED_FIXTURES {
        let parts = ArchiveParts::from_archive(&support::read_bytes(
            &support::fixtures_dir().join("archives").join(file),
        ));
        let decoded = parts.decoded_section(section);
        let rebuild = |mutated: &[u8]| {
            let mut mutated_parts = parts.clone();
            mutated_parts.set_decoded_section(section, mutated);
            mutated_parts.encode()
        };
        let description = format!("{file} {}", section.name());
        let fields = section_fields(section, &decoded, 0);
        mutate_structure(&decoded, &fields, &description, true, &rebuild).assert_no_panics();
    }
}

#[test]
fn mutated_metadata_sections_never_panic() {
    for file in STRUCTURED_FIXTURES {
        let parts = ArchiveParts::from_archive(&support::read_bytes(
            &support::fixtures_dir().join("archives").join(file),
        ));
        let rebuild = |mutated: &[u8]| parts.encode_with_metadata(mutated);
        let decoded = parts.decoded_metadata();
        mutate_structure(
            &decoded,
            &metadata_fields(&decoded),
            &format!("{file} metadata"),
            true,
            &rebuild,
        )
        .assert_no_panics();
    }
}

#[test]
fn mutated_schema_trees_never_panic() {
    mutate_section_of_every_fixture(SectionId::SchemaTree);
}

#[test]
fn mutated_schema_maps_never_panic() {
    mutate_section_of_every_fixture(SectionId::SchemaMap);
}

#[test]
fn mutated_table_metadata_never_panics() {
    mutate_section_of_every_fixture(SectionId::TableMetadata);
}

#[test]
fn mutated_dictionaries_never_panic() {
    for section in [
        SectionId::VariableDictionary,
        SectionId::LogTypeDictionary,
        SectionId::ArrayDictionary,
    ] {
        for file in STRUCTURED_FIXTURES {
            let parts = ArchiveParts::from_archive(&support::read_bytes(
                &support::fixtures_dir().join("archives").join(file),
            ));
            let decoded = parts.decoded_section(section);
            if decoded.is_empty() {
                continue;
            }
            let rebuild = |mutated: &[u8]| {
                let mut mutated_parts = parts.clone();
                mutated_parts.set_decoded_section(section, mutated);
                mutated_parts.encode()
            };
            let description = format!("{file} {}", section.name());
            let count = &parts.raw_section(section)[..8];
            let num_entries = u64::from_le_bytes(count.try_into().expect("eight bytes"));
            let fields = section_fields(section, &decoded, num_entries);
            mutate_structure(&decoded, &fields, &description, true, &rebuild).assert_no_panics();

            let rebuild_count = |mutated: &[u8]| {
                let mut mutated_parts = parts.clone();
                let mut raw = mutated.to_vec();
                raw.resize(8, 0);
                raw.extend_from_slice(&parts.raw_section(section)[8..]);
                *mutated_parts.raw_section_mut(section) = raw;
                mutated_parts.encode()
            };
            let count_field = Field {
                offset: 0,
                width: 8,
                is_big_endian: false,
            };
            mutate_structure(
                count,
                &[count_field],
                &format!("{description} count"),
                false,
                &rebuild_count,
            )
            .assert_no_panics();
        }
    }
}

#[test]
fn mutated_long_dictionary_entries_never_panic() -> anyhow::Result<()> {
    // The first entries keep the long-entry layout while bounding the run time.
    const MAX_NUM_ENTRIES: usize = 16;

    let parts = ArchiveParts::from_archive(&support::read_bytes(
        &support::fixtures_dir()
            .join("archives")
            .join("long-strings.default.clps"),
    ));
    let limits = OpenOptions::default().limits;
    for section in [SectionId::VariableDictionary, SectionId::LogTypeDictionary] {
        let decoded = parts.decoded_section(section);
        let num_entries = u64::from_le_bytes(parts.raw_section(section)[..8].try_into()?);
        let mut fields = section_fields(section, &decoded, num_entries);
        let prefix_size = fields
            .get(MAX_NUM_ENTRIES)
            .map_or(decoded.len(), |field| field.offset);
        fields.truncate(MAX_NUM_ENTRIES);
        let prefix = &decoded[..prefix_size];
        let load = |payload: &[u8]| -> anyhow::Result<()> {
            let mut raw = (fields.len() as u64).to_le_bytes().to_vec();
            raw.extend(zstd::encode_all(payload, 1)?);
            if SectionId::VariableDictionary == section {
                VariableDictionary::load(&raw, &limits)?;
            } else {
                LogTypeDictionary::load(&raw, &limits)?;
            }
            Ok(())
        };
        load(prefix)?;

        let mut outcomes = Outcomes::default();
        for field in &fields {
            for value in field.overwrites(prefix) {
                let mut mutated = prefix.to_vec();
                field.write(&mut mutated, value);
                outcomes.run(
                    &format!("{}: field at {} = {value:#x}", section.name(), field.offset),
                    || load(&mutated),
                );
            }
        }
        outcomes.assert_no_panics();
    }
    Ok(())
}

#[test]
fn mutated_archive_bytes_never_panic() {
    for path in support::readable_archives() {
        let bytes = support::read_bytes(&path);
        let name = path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        let mut outcomes = Outcomes::default();
        let header_positions = 0..support::to_usize(clp_s_format::ArchiveHeader::SIZE);
        for position in header_positions.chain(positions(bytes.len())) {
            for byte in byte_mutations(bytes[position]) {
                let mut mutated = bytes.to_vec();
                mutated[position] = byte;
                outcomes.read(
                    &SharedBytes::from(mutated),
                    &format!("{name}: byte {position} = {byte:#04x}"),
                );
            }
        }
        outcomes.assert_no_panics();
    }
}

#[test]
fn truncated_archives_fail() {
    for path in support::readable_archives() {
        let bytes = support::read_bytes(&path);
        let mut outcomes = Outcomes::default();
        for length in positions(bytes.len()) {
            let truncated = bytes.slice(..length);
            assert!(
                outcomes.read(
                    &truncated,
                    &format!("{}: truncated to {length}", path.display())
                ),
                "{}: truncation to {length} must fail",
                path.display()
            );

            if length < 64 {
                continue;
            }
            let mut patched = truncated.to_vec();
            patched[16..24].copy_from_slice(&(length as u64).to_le_bytes());
            assert!(
                outcomes.read(
                    &SharedBytes::from(patched),
                    &format!(
                        "{}: truncated to {length} with a patched header",
                        path.display()
                    ),
                ),
                "{}: truncation to {length} with a consistent header must fail",
                path.display()
            );
        }
        outcomes.assert_no_panics();
    }
}

#[test]
fn rejection_fixtures_never_panic_when_mutated() {
    let path = support::fixtures_dir().join("archives/reject/arrays.structurize-arrays.clps");
    let bytes = support::read_bytes(&path);
    let mut outcomes = Outcomes::default();
    for position in positions(bytes.len()) {
        for byte in byte_mutations(bytes[position]) {
            let mut mutated = bytes.to_vec();
            mutated[position] = byte;
            outcomes.read(
                &SharedBytes::from(mutated),
                &format!("byte {position} = {byte:#04x}"),
            );
        }
    }
    outcomes.assert_no_panics();
}
