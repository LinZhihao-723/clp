//! Shared helpers: fixture discovery, opening an archive with every table loaded, rendering
//! every record through [`RenderContext`] templates, and multiset comparison of JSON lines.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clp_s_format::DictionaryNeeds;
use clp_s_format::OpenOptions;
use clp_s_format::SharedBytes;
use clp_s_format::StreamDecoder;
use clp_s_format::TableBytes;
use clp_s_format::TableSet;
use clp_s_io::ArchiveStorage;
use clp_s_io::MemorySource;
use clp_s_io::ResolvedSource;
use clp_s_io::StreamReadOptions;
use clp_s_json::JsonOptions;
use clp_s_json::JsonRecordBuffer;
use clp_s_json::RenderContext;
use clp_s_resources::BufferPool;
use clp_s_resources::BufferPoolConfig;
use clp_s_resources::CancellationToken;
use clp_s_resources::MemoryBudget;
use clp_s_table::ArchiveTables;
use clp_s_table::ColumnDemand;
use clp_s_table::ColumnSet;
use clp_s_table::ResolvedProjection;
use clp_s_table::Table;
use clp_s_table::TableBatch;
use clp_s_table::TableScanner;

pub const EMULATED: JsonOptions = JsonOptions {
    should_emulate_cpp_plain_floats: true,
};

pub const SHORTEST: JsonOptions = JsonOptions {
    should_emulate_cpp_plain_floats: false,
};

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
/// The path of fixture archive `name` (e.g. `"search.ts"`).
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

/// # Returns
///
/// The lines of the JSON-lines file at `path`, without their newlines.
///
/// # Errors
///
/// Returns an error if the file cannot be read.
pub fn read_lines(path: &Path) -> anyhow::Result<Vec<Vec<u8>>> {
    let text = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(text
        .split(|byte| b'\n' == *byte)
        .filter(|line| !line.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

/// # Returns
///
/// The C++ `clp-s x` oracle lines of the fixture archive at `path`.
///
/// # Errors
///
/// Returns an error if the oracle cannot be read.
pub fn oracle_lines(path: &Path) -> anyhow::Result<Vec<Vec<u8>>> {
    read_lines(
        &fixtures_dir()
            .join("oracles")
            .join(format!("{}.jsonl", stem(path))),
    )
}

/// # Returns
///
/// The records of `left` missing from `right`, counting duplicates.
pub fn missing_from<'record>(left: &'record [Vec<u8>], right: &[Vec<u8>]) -> Vec<&'record [u8]> {
    let mut counts: HashMap<&[u8], i64> = HashMap::new();
    for record in right {
        *counts.entry(record).or_default() += 1;
    }
    let mut missing = Vec::new();
    for record in left {
        let count = counts.entry(record).or_default();
        if 0 == *count {
            missing.push(record.as_slice());
        } else {
            *count -= 1;
        }
    }
    missing
}

/// # Returns
///
/// Up to three of `records` as lossy text, for failure messages.
pub fn sample(records: &[&[u8]]) -> Vec<String> {
    records
        .iter()
        .take(3)
        .map(|record| String::from_utf8_lossy(record).into_owned())
        .collect()
}

/// One opened archive: its storage, table layer and render context.
pub struct OpenedArchive {
    pub storage: ArchiveStorage,
    pub tables: Arc<ArchiveTables>,
    pub context: RenderContext,
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
        Self::open_source(ResolvedSource::from_path(path)?)
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
        Self::open_source(ResolvedSource::Memory(MemorySource::new(bytes)))
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

    /// Renders every record of every table through templates of `projection` and `options`,
    /// scanning windows of `batch_num_rows` rows.
    ///
    /// # Returns
    ///
    /// The records, in table and row order, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if a table cannot be loaded, scanned or rendered.
    pub fn render(
        &self,
        projection: &ResolvedProjection,
        options: JsonOptions,
        batch_num_rows: u32,
    ) -> anyhow::Result<Vec<Vec<u8>>> {
        let mut records = Vec::new();
        self.for_each_record(projection, options, batch_num_rows, |record| {
            records.push(record.to_vec());
        })?;
        Ok(records)
    }

    /// Renders every record of every table like [`Self::render`], calling `visit` with each
    /// record (without its newline) instead of collecting them.
    ///
    /// # Errors
    ///
    /// Returns an error if a table cannot be loaded, scanned or rendered.
    pub fn for_each_record<VisitorType: FnMut(&[u8])>(
        &self,
        projection: &ResolvedProjection,
        options: JsonOptions,
        batch_num_rows: u32,
        mut visit: VisitorType,
    ) -> anyhow::Result<()> {
        let mut buffer = JsonRecordBuffer::new();
        for table in self.load_tables()? {
            let template = self
                .context
                .template(table.info().ordinal, projection, &options)?;
            scan_selected(
                &table,
                output_demand(&table, projection),
                batch_num_rows,
                |_| true,
                |batch| {
                    buffer.clear();
                    let num_written = template
                        .bind(batch, &self.context)?
                        .write_selected(&mut buffer)?;
                    anyhow::ensure!(
                        num_written as usize == buffer.len(),
                        "{num_written} records written, {} buffered",
                        buffer.len()
                    );
                    for index in 0..buffer.len() {
                        visit(buffer.record(index).context("the record exists")?);
                    }
                    Ok(())
                },
            )?;
        }
        Ok(())
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The opened archive of `source`, with every dictionary loaded, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if the archive cannot be opened or a dictionary cannot be loaded.
    fn open_source(source: ResolvedSource) -> anyhow::Result<Self> {
        let token = CancellationToken::new();
        let storage = ArchiveStorage::open(source, &OpenOptions::default(), &token)?;
        storage.load_dictionaries(
            DictionaryNeeds {
                is_variable_needed: true,
                is_logtype_needed: true,
                is_array_needed: true,
            },
            &token,
        )?;
        let tables = Arc::new(ArchiveTables::new(
            Arc::clone(storage.index()),
            Arc::clone(storage.dictionaries()),
        ));
        let context = RenderContext::new(Arc::clone(&tables));
        Ok(Self {
            storage,
            tables,
            context,
        })
    }
}

/// # Returns
///
/// The demand that decodes, as output-only columns, every column `projection` renders.
pub fn output_demand(table: &Table, projection: &ResolvedProjection) -> Arc<ColumnDemand> {
    let layout = table.layout();
    Arc::new(ColumnDemand::new(
        ColumnSet::empty(layout.num_columns()),
        projection.output_columns(layout),
    ))
}

/// Scans every window of `table` with `demand`, selects the rows `is_selected` accepts (by table
/// row), and calls `visit` with each finished batch that has a selected row.
///
/// # Errors
///
/// Returns an error if a window cannot be decoded or `visit` fails.
pub fn scan_selected<
    SelectorType: FnMut(u32) -> bool,
    VisitorType: FnMut(TableBatch<'_>) -> anyhow::Result<()>,
>(
    table: &Arc<Table>,
    demand: Arc<ColumnDemand>,
    batch_num_rows: u32,
    mut is_selected: SelectorType,
    mut visit: VisitorType,
) -> anyhow::Result<()> {
    let mut scanner = TableScanner::new(NonZeroU32::new(batch_num_rows).context("non-zero")?);
    let mut state = scanner.start(Arc::clone(table), demand);
    while let Some(window) = scanner.next_window(&mut state)? {
        let first_row = window.columns.first_row();
        for row in 0..window.columns.num_rows() {
            if is_selected(first_row + row) {
                window.selected.select(row);
            }
        }
        // Like the scan driver, skip windows without a selected row: their output-only columns
        // are not decoded.
        if 0 < scanner.finish_window(&state)? {
            visit(scanner.batch(&state))?;
        }
    }
    anyhow::ensure!(state.is_finished(), "the scan finished");
    Ok(())
}
