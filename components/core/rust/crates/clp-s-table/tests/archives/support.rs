//! Shared helpers: fixture discovery, opening an archive with every table loaded, a test-only JSON
//! renderer built on [`RecordVisitor`], full scans, and an archive rebuilder for mutations.

use std::num::NonZeroU32;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clp_s_format::ArchiveHeader;
use clp_s_format::ArchiveOpener;
use clp_s_format::DictionaryNeeds;
use clp_s_format::OpenOptions;
use clp_s_format::OpenStep;
use clp_s_format::SectionId;
use clp_s_format::SharedBytes;
use clp_s_format::StreamDecoder;
use clp_s_format::TableBytes;
use clp_s_format::TableSet;
use clp_s_format::TimestampIndex;
use clp_s_io::ArchiveStorage;
use clp_s_io::MemorySource;
use clp_s_io::ResolvedSource;
use clp_s_io::StreamReadOptions;
use clp_s_resources::BufferPool;
use clp_s_resources::BufferPoolConfig;
use clp_s_resources::CancellationToken;
use clp_s_resources::MemoryBudget;
use clp_s_table::ArchiveTables;
use clp_s_table::ColumnDemand;
use clp_s_table::ColumnSet;
use clp_s_table::FloatRef;
use clp_s_table::RecordVisitor;
use clp_s_table::StringRef;
use clp_s_table::Table;
use clp_s_table::TableBatch;
use clp_s_table::TableScanner;
use clp_s_table::ValueRef;

use crate::json::Json;

pub const ALL_DICTIONARIES: DictionaryNeeds = DictionaryNeeds {
    is_variable_needed: true,
    is_logtype_needed: true,
    is_array_needed: true,
};

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
                .is_some_and(|extension| "clps" == extension)
        })
        .collect();
    archives.sort();
    assert!(!archives.is_empty(), "the fixture corpus has archives");
    archives
}

/// # Returns
///
/// The path of fixture archive `name` (e.g. `"logs.ts"`).
pub fn fixture(name: &str) -> PathBuf {
    fixtures_dir().join("archives").join(format!("{name}.clps"))
}

/// # Returns
///
/// The file stem of `path`.
pub fn stem(path: &Path) -> &str {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .expect("fixture names are UTF-8")
}

/// One opened archive: its storage and table layer.
pub struct OpenedArchive {
    pub storage: ArchiveStorage,
    pub tables: Arc<ArchiveTables>,
}

impl OpenedArchive {
    /// Factory function.
    ///
    /// Opens the archive file at `path` and loads every dictionary.
    ///
    /// # Returns
    ///
    /// The opened archive on success.
    ///
    /// # Errors
    ///
    /// Returns an error if the archive cannot be opened or a dictionary cannot be loaded.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let source = ResolvedSource::from_path(path)?;
        Self::open_source(source, ALL_DICTIONARIES)
    }

    /// Factory function.
    ///
    /// Opens in-memory archive bytes and loads every dictionary.
    ///
    /// # Returns
    ///
    /// The opened archive on success.
    ///
    /// # Errors
    ///
    /// Returns an error if the archive cannot be opened or a dictionary cannot be loaded.
    pub fn open_bytes(bytes: SharedBytes) -> anyhow::Result<Self> {
        Self::open_source(
            ResolvedSource::Memory(MemorySource::new(bytes)),
            ALL_DICTIONARIES,
        )
    }

    /// Factory function.
    ///
    /// Opens `source` and loads the dictionaries of `needs`.
    ///
    /// # Returns
    ///
    /// The opened archive on success.
    ///
    /// # Errors
    ///
    /// Returns an error if the archive cannot be opened or a dictionary cannot be loaded.
    pub fn open_source(source: ResolvedSource, needs: DictionaryNeeds) -> anyhow::Result<Self> {
        let token = CancellationToken::new();
        let storage = ArchiveStorage::open(source, &OpenOptions::default(), &token)?;
        storage.load_dictionaries(needs, &token)?;
        let tables = Arc::new(ArchiveTables::new(
            Arc::clone(storage.index()),
            Arc::clone(storage.dictionaries()),
        ));
        Ok(Self { storage, tables })
    }

    /// Decodes every packed stream.
    ///
    /// # Returns
    ///
    /// The bytes of every table, in directory order, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if a stream cannot be read or a table lies outside its stream.
    pub fn table_bytes(&self) -> anyhow::Result<Vec<TableBytes>> {
        let index = self.storage.index();
        let num_tables = u32::try_from(index.tables().tables().len())?;
        let plan = index.plan_reads(&TableSet::full(num_tables), DictionaryNeeds::default());
        let pool = BufferPool::new(BufferPoolConfig::default());
        let budget = MemoryBudget::new(None);
        let mut decoder = StreamDecoder::new()?;
        let mut tables = Vec::new();
        for read in &plan.streams {
            let buffer = pool.acquire(
                read.decoded_prefix_size,
                budget.try_reserve(read.decoded_prefix_size)?,
            )?;
            let stream = self.storage.read_stream(
                read,
                &mut decoder,
                buffer,
                &StreamReadOptions::default(),
            )?;
            for ordinal in &read.tables {
                let info = *index
                    .tables()
                    .table(*ordinal)
                    .context("planned tables exist")?;
                tables.push(TableBytes::new(&stream, info)?);
            }
        }
        tables.sort_by_key(|bytes| bytes.info().ordinal);
        Ok(tables)
    }

    /// Loads every table.
    ///
    /// # Returns
    ///
    /// The tables, in directory order, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if a stream cannot be read or a table fails to load.
    pub fn load_tables(&self) -> anyhow::Result<Vec<Arc<Table>>> {
        self.table_bytes()?
            .into_iter()
            .map(|bytes| Ok(Arc::new(Table::load(Arc::clone(&self.tables), bytes)?)))
            .collect()
    }
}

/// # Returns
///
/// The demand that decodes every column of `table` for evaluation.
pub fn full_demand(table: &Table) -> Arc<ColumnDemand> {
    let num_columns = table.layout().num_columns();
    Arc::new(ColumnDemand::new(
        ColumnSet::full(num_columns),
        ColumnSet::empty(num_columns),
    ))
}

/// Scans every window of `table` with `demand`, selecting every row, and calls `visit` with each
/// finished batch.
///
/// # Errors
///
/// Returns an error if a window cannot be decoded or `visit` fails.
pub fn scan_all<VisitorType: FnMut(TableBatch<'_>) -> anyhow::Result<()>>(
    table: &Arc<Table>,
    demand: Arc<ColumnDemand>,
    batch_num_rows: u32,
    mut visit: VisitorType,
) -> anyhow::Result<()> {
    let mut scanner = TableScanner::new(NonZeroU32::new(batch_num_rows).context("non-zero")?);
    let mut state = scanner.start(Arc::clone(table), demand);
    while let Some(window) = scanner.next_window(&mut state)? {
        window.selected.select_all();
        scanner.finish_window(&state)?;
        visit(scanner.batch(&state))?;
    }
    anyhow::ensure!(state.is_finished(), "the scan finished");
    Ok(())
}

/// Renders every record of every table of `archive` with windows of `batch_num_rows` rows.
///
/// # Returns
///
/// The records in table order on success.
///
/// # Errors
///
/// Returns an error if a table cannot be loaded, scanned or rendered.
pub fn render_archive(archive: &OpenedArchive, batch_num_rows: u32) -> anyhow::Result<Vec<Json>> {
    let timestamps = archive.storage.index().metadata().timestamp_index();
    let mut records = Vec::new();
    for table in archive.load_tables()? {
        scan_all(&table, full_demand(&table), batch_num_rows, |batch| {
            for row in 0..batch.num_rows() {
                records.push(Renderer::render(&batch, row, timestamps)?);
            }
            Ok(())
        })?;
    }
    Ok(records)
}

/// A test-only renderer: builds the [`Json`] tree of a record with the same number and string
/// forms C++ `clp-s x` prints, so it can be compared with the oracles.
pub struct Renderer<'index> {
    timestamps: &'index TimestampIndex,

    /// The open objects, innermost last.
    stack: Vec<OpenObject>,

    record: Option<Json>,
}

impl<'index> Renderer<'index> {
    /// # Returns
    ///
    /// The tree of record `row` of `batch` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if the record cannot be walked or a value cannot be rendered.
    pub fn render(
        batch: &TableBatch<'_>,
        row: u32,
        timestamps: &'index TimestampIndex,
    ) -> anyhow::Result<Json> {
        let mut renderer = Self {
            timestamps,
            stack: Vec::new(),
            record: None,
        };
        batch.record(row).walk(&mut renderer)?;
        renderer.record.context("the walk closed the record")
    }

    /// # Returns
    ///
    /// The tree of `value` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if the value cannot be decoded or formatted.
    fn value_tree(&self, value: ValueRef<'_>) -> anyhow::Result<Json> {
        let mut text = Vec::new();
        Ok(match value {
            ValueRef::Null => Json::Null,
            ValueRef::Boolean(value) => Json::Bool(value),
            ValueRef::Integer(value) => Json::Number(value.to_string()),
            ValueRef::Float(FloatRef::Plain(value)) => Json::number(&format!("{value:.6}")),
            ValueRef::Float(FloatRef::Formatted { value, .. }) => {
                Json::number(&format!("{value:?}"))
            }
            ValueRef::Float(FloatRef::Dictionary { text, .. }) => {
                Json::number(std::str::from_utf8(text)?)
            }
            ValueRef::String(StringRef::Variable { text, .. }) => Json::String(text.to_vec()),
            ValueRef::String(StringRef::Encoded(encoded)) => {
                encoded.decode(&mut text)?;
                Json::String(text)
            }
            ValueRef::Timestamp {
                epoch_ns,
                pattern_id,
            } => {
                self.timestamps
                    .pattern(pattern_id)?
                    .format_json(epoch_ns, &mut text)?;
                Json::parse(&text)?
            }
            ValueRef::UnstructuredArray(encoded) => {
                encoded.decode(&mut text)?;
                Json::parse(&text)?
            }
            ValueRef::EmptyObject => Json::Object(Vec::new()),
            _ => anyhow::bail!("unknown value {value:?}"),
        })
    }
}

impl RecordVisitor for Renderer<'_> {
    type Error = anyhow::Error;

    fn begin_object(&mut self, key: Option<&[u8]>) -> Result<(), Self::Error> {
        self.stack.push(OpenObject {
            key: key.map(<[u8]>::to_vec),
            members: Vec::new(),
        });
        Ok(())
    }

    fn end_object(&mut self) -> Result<(), Self::Error> {
        let OpenObject { key, members } = self.stack.pop().context("an object is open")?;
        let object = Json::Object(members);
        match (key, self.stack.last_mut()) {
            (Some(key), Some(parent)) => parent.members.push((key, object)),
            (None, None) => self.record = Some(object),
            _ => anyhow::bail!("unbalanced objects"),
        }
        Ok(())
    }

    fn value(&mut self, key: &[u8], value: ValueRef<'_>) -> Result<(), Self::Error> {
        let tree = self.value_tree(value)?;
        self.stack
            .last_mut()
            .context("an object is open")?
            .members
            .push((key.to_vec(), tree));
        Ok(())
    }
}

/// # Returns
///
/// The serialized records of the C++ oracle of the archive at `path`, in file order.
///
/// # Errors
///
/// Returns an error if the oracle cannot be read or a line is not JSON.
pub fn oracle_records(path: &Path) -> anyhow::Result<Vec<Json>> {
    let oracle = fixtures_dir()
        .join("oracles")
        .join(format!("{}.jsonl", stem(path)));
    let text = std::fs::read(&oracle).with_context(|| format!("read {}", oracle.display()))?;
    text.split(|byte| b'\n' == *byte)
        .filter(|line| !line.is_empty())
        .map(Json::parse)
        .collect()
}

/// An archive taken apart into header fields, metadata packets and raw sections, so tests can
/// change one structure and reassemble a consistent archive.
pub struct ArchiveParts {
    header_prefix: [u8; 8],
    uncompressed_size: u64,
    reserved: [u8; 32],
    compression_type: u16,
    packets: Vec<(u8, Vec<u8>)>,
    sections: Vec<(SectionId, Vec<u8>)>,
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
    /// The decoded payload of the (non-dictionary) section `section`.
    pub fn decoded_section(&self, section: SectionId) -> Vec<u8> {
        zstd::decode_all(self.raw_section(section)).expect("the section frame decodes")
    }

    /// Replaces the decoded payload of the (non-dictionary) section `section`.
    pub fn set_decoded_section(&mut self, section: SectionId, decoded: &[u8]) {
        *self.raw_section_mut(section) = zstd::encode_all(decoded, 3).expect("zstd compresses");
    }

    /// # Returns
    ///
    /// The payload of the first metadata packet of type `packet_type`, mutably.
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
    /// The reassembled archive.
    pub fn encode(&self) -> SharedBytes {
        let mut decoded_metadata = vec![u8::try_from(self.packets.len()).expect("few packets")];
        for (packet_type, payload) in &self.packets {
            let file_info;
            let payload = if FILE_INFO_PACKET_TYPE == *packet_type {
                file_info = self.encode_file_info();
                &file_info
            } else {
                payload
            };
            decoded_metadata.push(*packet_type);
            decoded_metadata.extend_from_slice(
                &u32::try_from(payload.len())
                    .expect("small payload")
                    .to_le_bytes(),
            );
            decoded_metadata.extend_from_slice(payload);
        }
        let metadata = zstd::encode_all(decoded_metadata.as_slice(), 3).expect("zstd compresses");
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
    /// The raw bytes of `section`.
    fn raw_section(&self, section: SectionId) -> &[u8] {
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
    fn raw_section_mut(&mut self, section: SectionId) -> &mut Vec<u8> {
        &mut self
            .sections
            .iter_mut()
            .find(|(id, _)| *id == section)
            .expect("every section exists")
            .1
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
/// `value` as a `usize`.
pub fn to_usize(value: u64) -> usize {
    usize::try_from(value).expect("the value fits in memory")
}

/// An object a [`Renderer`] has opened but not closed.
struct OpenObject {
    /// The key the object is stored under, or `None` for the record root.
    key: Option<Vec<u8>>,

    members: Vec<(Vec<u8>, Json)>,
}
