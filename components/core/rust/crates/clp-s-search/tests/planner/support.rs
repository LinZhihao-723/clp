//! Shared helpers: fixture discovery, archive opening, the planner driver, per-row reference
//! inputs, and a scalar evaluator of residual filters that follows the binding contract of
//! `clp_s_search::ResidualFilter`.

use std::num::NonZeroU32;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clp_s_format::DictionaryNeeds;
use clp_s_format::MetadataFields;
use clp_s_format::MetadataValue;
use clp_s_format::OpenOptions;
use clp_s_format::StreamDecoder;
use clp_s_format::TableBytes;
use clp_s_format::TableSet;
use clp_s_io::ArchiveStorage;
use clp_s_io::ResolvedSource;
use clp_s_io::StreamReadOptions;
use clp_s_query::ColumnPattern;
use clp_s_query::ColumnRef;
use clp_s_query::JsonRecord;
use clp_s_query::Leaf;
use clp_s_query::Namespace;
use clp_s_query::NormalExpr;
use clp_s_query::PathToken;
use clp_s_query::PredicateTest;
use clp_s_query::Query;
use clp_s_query::RecordValueSource;
use clp_s_query::ScalarValue;
use clp_s_query::TruthValue;
use clp_s_resources::BufferPool;
use clp_s_resources::BufferPoolConfig;
use clp_s_resources::CancellationToken;
use clp_s_resources::MemoryBudget;
use clp_s_search::ArchiveSearchPlan;
use clp_s_search::ArrayAccess;
use clp_s_search::ColumnTest;
use clp_s_search::EncodedTextMatch;
use clp_s_search::EncodedTextMember;
use clp_s_search::LeafBinding;
use clp_s_search::PlanLimits;
use clp_s_search::PlanStep;
use clp_s_search::PruneReport;
use clp_s_search::RecordProjection;
use clp_s_search::ResidualFilter;
use clp_s_search::SearchPlanner;
use clp_s_table::ArchiveTables;
use clp_s_table::ColumnDemand;
use clp_s_table::ColumnIndex;
use clp_s_table::ColumnSet;
use clp_s_table::FloatRef;
use clp_s_table::RecordVisitor;
use clp_s_table::StringRef;
use clp_s_table::Table;
use clp_s_table::TableScanner;
use clp_s_table::ValueRef;

pub const ALL_DICTIONARIES: DictionaryNeeds = DictionaryNeeds {
    is_variable_needed: true,
    is_logtype_needed: true,
    is_array_needed: true,
};

/// # Returns
///
/// The prototype workspace directory.
pub fn prototype_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// # Returns
///
/// The directory of large artifacts: `$PROTO_DATA_ROOT`, else the workspace's `target/proto-data`.
pub fn data_root() -> PathBuf {
    std::env::var_os("PROTO_DATA_ROOT").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/proto-data"),
        PathBuf::from,
    )
}

/// # Returns
///
/// Every readable fixture archive, sorted by file name.
pub fn readable_archives() -> Vec<PathBuf> {
    let mut archives: Vec<PathBuf> =
        std::fs::read_dir(prototype_dir().join("tests/fixtures/archives"))
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
    prototype_dir()
        .join("tests/fixtures/archives")
        .join(format!("{name}.clps"))
}

/// # Returns
///
/// The file stem of `path`.
pub fn stem(path: &Path) -> &str {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .expect("fixture names are UTF-8")
}

/// One opened archive: its storage and table layer; dictionaries load on demand.
pub struct OpenedArchive {
    pub storage: ArchiveStorage,
    pub tables: Arc<ArchiveTables>,
}

impl OpenedArchive {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The archive at `path`, with no dictionary loaded, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`ResolvedSource::from_path`]'s return values on failure.
    /// * Forwards [`ArchiveStorage::open`]'s return values on failure.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let storage = ArchiveStorage::open(
            ResolvedSource::from_path(path)?,
            &OpenOptions::default(),
            &CancellationToken::new(),
        )?;
        let tables = Arc::new(ArchiveTables::new(
            Arc::clone(storage.index()),
            Arc::clone(storage.dictionaries()),
        ));
        Ok(Self { storage, tables })
    }

    /// Loads the dictionaries of `needs`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`ArchiveStorage::load_dictionaries`]'s return values on failure.
    pub fn load(&self, needs: DictionaryNeeds) -> anyhow::Result<()> {
        self.storage
            .load_dictionaries(needs, &CancellationToken::new())?;
        Ok(())
    }

    /// Loads every table.
    ///
    /// # Returns
    ///
    /// The tables, in directory order, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`ArchiveStorage::read_stream`]'s return values on failure.
    /// * Forwards [`TableBytes::new`]'s return values on failure.
    /// * Forwards [`Table::load`]'s return values on failure.
    pub fn load_tables(&self) -> anyhow::Result<Vec<Arc<Table>>> {
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
                let bytes = TableBytes::new(&stream, info)?;
                tables.push(Arc::new(Table::load(Arc::clone(&self.tables), bytes)?));
            }
        }
        tables.sort_by_key(|table| table.info().ordinal);
        Ok(tables)
    }
}

/// The outcome of planning one archive.
pub enum Planned {
    Pruned(PruneReport),
    Plan(Arc<ArchiveSearchPlan>),
}

/// Plans `query` against `archive`, loading the dictionaries the planner asks for.
///
/// # Returns
///
/// The outcome and the number of dictionary requests, on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`SearchPlanner::new`]'s return values on failure.
/// * Forwards [`clp_s_search::ArchivePlanner::advance`]'s return values on failure.
/// * Forwards [`OpenedArchive::load`]'s return values on failure.
pub fn plan(
    archive: &OpenedArchive,
    query: &Arc<Query>,
    projection: &RecordProjection,
) -> anyhow::Result<(Planned, usize)> {
    let planner = SearchPlanner::new(Arc::clone(query), projection, PlanLimits::default())?;
    if let Some(report) = planner.prune_on_metadata(archive.storage.index().metadata()) {
        return Ok((Planned::Pruned(report), 0));
    }
    let mut archive_planner = planner.plan_archive(Arc::clone(&archive.tables));
    let mut num_requests = 0;
    loop {
        match archive_planner.advance()? {
            PlanStep::NeedsDictionaries(needs) => {
                num_requests += 1;
                anyhow::ensure!(
                    num_requests < 3,
                    "the planner keeps asking for dictionaries"
                );
                archive.load(needs)?;
            }
            PlanStep::Pruned(report) => return Ok((Planned::Pruned(report), num_requests)),
            PlanStep::Planned(plan) => return Ok((Planned::Plan(plan), num_requests)),
        }
    }
}

/// One stored column value of a row, owned.
#[derive(Clone, Debug)]
pub enum StoredValue {
    Scalar(OwnedScalar),
    Variable(u64),
    Encoded {
        logtype: u64,
        variables: Vec<i64>,
        text: Vec<u8>,
    },
    Array(serde_json::Value),
}

/// An owned scalar for single-value evaluation.
#[derive(Clone, Copy, Debug)]
pub enum OwnedScalar {
    Integer(i64),
    Float(f64),
    Boolean(bool),
    Timestamp(i64),
}

/// The reference inputs and stored columns of one record.
pub struct Row {
    pub record: serde_json::Value,
    pub timestamp_ns: Option<i64>,
    pub log_event_idx: Option<u64>,
    pub range_fields: Option<serde_json::Value>,
    pub columns: Vec<Option<StoredValue>>,

    /// Why the reference evaluator cannot see this record exactly as stored, if it cannot.
    pub unverifiable: Option<&'static str>,
}

/// The rows of every table of one archive.
pub struct ArchiveRows {
    pub timestamp_key: Option<ColumnPattern>,
    pub tables: Vec<Vec<Row>>,
}

impl ArchiveRows {
    /// Factory function.
    ///
    /// Decodes every row of every table of the archive at `path`.
    ///
    /// # Returns
    ///
    /// The rows on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::for_each_table`]'s return values on failure.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut tables = Vec::new();
        let timestamp_key = Self::for_each_table(path, |_, rows| {
            tables.push(rows);
            Ok(())
        })?;
        Ok(Self {
            timestamp_key,
            tables,
        })
    }

    /// Decodes the rows of every table of the archive at `path`, one table at a time, and calls
    /// `visit` with the archive's timestamp key and each table's rows, in directory order.
    ///
    /// # Type Parameters
    ///
    /// * `VisitorType` - Receives the timestamp key and the rows of one table.
    ///
    /// # Returns
    ///
    /// The archive's timestamp key on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`OpenedArchive::open`]'s return values on failure.
    /// * Forwards [`OpenedArchive::load_tables`]'s return values on failure.
    /// * Forwards [`clp_s_table::TableScanner::next_window`]'s return values on failure.
    /// * Forwards [`clp_s_table::RecordView::walk`]'s return values on failure.
    /// * Forwards [`stored_value`]'s return values on failure.
    /// * Forwards `visit`'s return values on failure.
    pub fn for_each_table<
        VisitorType: FnMut(Option<&ColumnPattern>, Vec<Row>) -> anyhow::Result<()>,
    >(
        path: &Path,
        mut visit: VisitorType,
    ) -> anyhow::Result<Option<ColumnPattern>> {
        let archive = OpenedArchive::open(path)?;
        archive.load(ALL_DICTIONARIES)?;
        let index = archive.storage.index();
        let timestamp_key = index
            .metadata()
            .timestamp_index()
            .authoritative()
            .and_then(|entry| std::str::from_utf8(&entry.key).ok())
            .and_then(|key| ColumnPattern::parse(key).ok());
        let range_index = index.metadata().range_index();
        for table in archive.load_tables()? {
            let layout = Arc::clone(table.layout());
            let num_columns = layout.num_columns();
            let demand = Arc::new(ColumnDemand::new(
                ColumnSet::full(num_columns),
                ColumnSet::empty(num_columns),
            ));
            let mut scanner = TableScanner::new(NonZeroU32::new(4096).context("non-zero")?);
            let mut state = scanner.start(Arc::clone(&table), demand);
            let mut rows = Vec::new();
            while let Some(window) = scanner.next_window(&mut state)? {
                window.selected.select_all();
                scanner.finish_window(&state)?;
                let batch = scanner.batch(&state);
                for row in 0..batch.num_rows() {
                    let mut builder = RecordBuilder::default();
                    batch.record(row).walk(&mut builder)?;
                    let record = builder.record.context("the walk closed the record")?;
                    let log_event_idx = batch
                        .log_event_idx(row)?
                        .and_then(|value| u64::try_from(value).ok());
                    let range_fields = log_event_idx
                        .and_then(|idx| range_index.and_then(|ranges| ranges.entry_containing(idx)))
                        .map(|entry| fields_to_json(&entry.fields));
                    let mut columns = Vec::with_capacity(num_columns as usize);
                    let mut unverifiable = builder.unverifiable;
                    for column in 0..num_columns {
                        let value = batch.column(ColumnIndex::new(column))?.value(row)?;
                        let (stored, problem) = stored_value(value)?;
                        unverifiable = unverifiable.or(problem);
                        columns.push(stored);
                    }
                    rows.push(Row {
                        record,
                        timestamp_ns: batch.record_timestamp_ns(row)?,
                        log_event_idx,
                        range_fields,
                        columns,
                        unverifiable,
                    });
                }
            }
            visit(timestamp_key.as_ref(), rows)?;
        }
        Ok(timestamp_key)
    }

    /// # Returns
    ///
    /// Whether the reference evaluator matches `row` with `query`.
    pub fn reference_matches(&self, query: &Query, row: &Row) -> bool {
        Self::reference_matches_with(self.timestamp_key.as_ref(), query, row)
    }

    /// # Returns
    ///
    /// Whether the reference evaluator matches `row` of an archive with `timestamp_key` with
    /// `query`.
    pub fn reference_matches_with(
        timestamp_key: Option<&ColumnPattern>,
        query: &Query,
        row: &Row,
    ) -> bool {
        let timestamp = row
            .timestamp_ns
            .map(|epoch_ns| ScalarValue::Timestamp { epoch_ns });
        let mut source = JsonRecord::new(&row.record, timestamp);
        if let Some(key) = timestamp_key {
            source = source.with_timestamp_key(key);
        }
        if let Some(fields) = &row.range_fields {
            source = source.with_range_index_fields(fields);
        }
        TruthValue::True == query.evaluate(&source)
    }
}

/// Evaluates a residual filter on one row with the binding contract, scalar by scalar.
pub struct ResidualEvaluator<'plan> {
    pub query: &'plan Query,
    pub residual: &'plan ResidualFilter,

    /// Problems found in the prefilters: a pattern match the prefilter rejected, or a value the
    /// prefilter accepted without a glob check that does not match.
    pub prefilter_violations: Vec<String>,
}

impl ResidualEvaluator<'_> {
    /// # Returns
    ///
    /// Whether `row` matches the residual.
    pub fn matches(&mut self, row: &Row) -> bool {
        self.node_matches(self.residual.root(), row)
    }

    /// # Returns
    ///
    /// Whether `node` is True on `row`.
    fn node_matches(&mut self, node: &NormalExpr, row: &Row) -> bool {
        match node {
            NormalExpr::MatchAll => true,
            NormalExpr::MatchNone => false,
            NormalExpr::And(children) => children.iter().all(|child| self.node_matches(child, row)),
            NormalExpr::Or(children) => children.iter().any(|child| self.node_matches(child, row)),
            NormalExpr::Leaf { leaf, is_negated } => {
                let Some(binding) = self.residual.binding(*leaf) else {
                    self.prefilter_violations
                        .push(format!("leaf {} has no binding", leaf.get()));
                    return false;
                };
                let leaf = &self.query.leaves()[leaf.get() as usize];
                let expected = if *is_negated {
                    TruthValue::False
                } else {
                    TruthValue::True
                };
                expected == self.leaf_value(leaf, binding, row)
            }
        }
    }

    /// # Returns
    ///
    /// The existential join of the binding's columns on `row`.
    fn leaf_value(&mut self, leaf: &Leaf, binding: &LeafBinding, row: &Row) -> TruthValue {
        let mut is_any_true = false;
        let mut is_any_false = binding.is_false_without_columns;
        for column in &binding.columns {
            match self.column_value(
                leaf,
                &column.test,
                row.columns
                    .get(column.column.get() as usize)
                    .and_then(Option::as_ref),
                row,
            ) {
                TruthValue::True => is_any_true = true,
                TruthValue::False => is_any_false = true,
                TruthValue::Unknown => {}
            }
        }
        if is_any_true {
            TruthValue::True
        } else if is_any_false {
            TruthValue::False
        } else {
            TruthValue::Unknown
        }
    }

    /// # Returns
    ///
    /// The value of one bound column on `row`.
    fn column_value(
        &mut self,
        leaf: &Leaf,
        test: &ColumnTest,
        stored: Option<&StoredValue>,
        row: &Row,
    ) -> TruthValue {
        match (test, stored) {
            (ColumnTest::Value, Some(StoredValue::Scalar(scalar))) => {
                let value = match *scalar {
                    OwnedScalar::Integer(value) => ScalarValue::Integer(value),
                    OwnedScalar::Float(value) => ScalarValue::Float(value),
                    OwnedScalar::Boolean(value) => ScalarValue::Boolean(value),
                    OwnedScalar::Timestamp(epoch_ns) => ScalarValue::Timestamp { epoch_ns },
                };
                leaf.evaluate(&SingleValue(value))
            }
            (ColumnTest::VariableIds(ids), Some(StoredValue::Variable(id))) => {
                truth(ids.contains(*id))
            }
            (
                ColumnTest::EncodedText(members),
                Some(StoredValue::Encoded {
                    logtype,
                    variables,
                    text,
                }),
            ) => {
                let is_case_sensitive = match &leaf.test {
                    PredicateTest::Compare {
                        is_case_sensitive, ..
                    }
                    | PredicateTest::InSet {
                        is_case_sensitive, ..
                    } => *is_case_sensitive,
                    _ => true,
                };
                let mut is_match = false;
                for member in members.iter() {
                    is_match |=
                        self.member_matches(member, *logtype, variables, text, is_case_sensitive);
                }
                truth(is_match)
            }
            (ColumnTest::Array(access), Some(StoredValue::Array(array))) => {
                array_value(leaf, access, array)
            }
            (ColumnTest::LogEventRanges(ranges), _) => row
                .log_event_idx
                .and_then(|idx| ranges.outcome(idx))
                .map_or(TruthValue::Unknown, truth),
            (test, stored) => {
                self.prefilter_violations.push(format!(
                    "test {test:?} is bound to the stored value {stored:?}"
                ));
                TruthValue::Unknown
            }
        }
    }

    /// # Returns
    ///
    /// Whether an encoded text value matches `member` by the prefilter contract, recording
    /// contract violations.
    fn member_matches(
        &mut self,
        member: &EncodedTextMember,
        logtype: u64,
        variables: &[i64],
        text: &[u8],
        is_case_sensitive: bool,
    ) -> bool {
        let is_glob_match = member.pattern.matches(text, is_case_sensitive);
        let verdict = match &member.prefilter {
            EncodedTextMatch::Never => false,
            EncodedTextMatch::DecodeAll => is_glob_match,
            EncodedTextMatch::Subqueries(subqueries) => {
                let matching: Vec<_> = subqueries
                    .iter()
                    .filter(|subquery| {
                        subquery.logtypes.contains(logtype)
                            && matches_in_order(&subquery.variables, variables)
                    })
                    .collect();
                if matching.is_empty() {
                    false
                } else if matching
                    .iter()
                    .any(|subquery| !subquery.is_glob_check_required)
                {
                    true
                } else {
                    is_glob_match
                }
            }
        };
        if verdict != is_glob_match {
            self.prefilter_violations.push(format!(
                "prefilter of {:?} says {verdict} on {:?} (logtype {logtype}, variables \
                 {variables:?}), the glob says {is_glob_match}",
                member.pattern,
                String::from_utf8_lossy(text)
            ));
        }
        verdict
    }
}

/// A record source that reaches one value whatever the column.
struct SingleValue<'value>(ScalarValue<'value>);

impl RecordValueSource for SingleValue<'_> {
    fn visit_values<VisitorType: FnMut(ScalarValue<'_>) -> bool>(
        &self,
        _column: &ColumnRef,
        mut visit: VisitorType,
    ) {
        visit(self.0);
    }
}

/// Builds the default-namespace JSON of one record.
#[derive(Default)]
struct RecordBuilder {
    stack: Vec<(Option<String>, serde_json::Map<String, serde_json::Value>)>,
    record: Option<serde_json::Value>,
    unverifiable: Option<&'static str>,
}

impl RecordBuilder {
    /// # Returns
    ///
    /// The UTF-8 text of `bytes`, flagging the record if it is not UTF-8.
    fn text(&mut self, bytes: &[u8]) -> String {
        std::str::from_utf8(bytes).map_or_else(
            |_| {
                self.unverifiable = Some("invalid UTF-8");
                String::from_utf8_lossy(bytes).into_owned()
            },
            str::to_owned,
        )
    }
}

impl RecordVisitor for RecordBuilder {
    type Error = anyhow::Error;

    fn begin_object(&mut self, key: Option<&[u8]>) -> Result<(), Self::Error> {
        let key = key.map(|key| self.text(key));
        self.stack.push((key, serde_json::Map::new()));
        Ok(())
    }

    fn end_object(&mut self) -> Result<(), Self::Error> {
        let (key, members) = self.stack.pop().context("an object is open")?;
        let object = serde_json::Value::Object(members);
        match (key, self.stack.last_mut()) {
            (Some(key), Some((_, parent))) => {
                parent.insert(key, object);
            }
            (None, None) => self.record = Some(object),
            _ => anyhow::bail!("unbalanced objects"),
        }
        Ok(())
    }

    fn value(&mut self, key: &[u8], value: ValueRef<'_>) -> Result<(), Self::Error> {
        let key = self.text(key);
        let mut decoded = Vec::new();
        let json = match value {
            ValueRef::Null => serde_json::Value::Null,
            ValueRef::Boolean(value) => serde_json::Value::Bool(value),
            ValueRef::Integer(value)
            | ValueRef::Timestamp {
                epoch_ns: value, ..
            } => value.into(),
            ValueRef::Float(float) => {
                let value = float_value(float)?;
                serde_json::Number::from_f64(value).map_or_else(
                    || {
                        self.unverifiable = Some("non-finite float");
                        serde_json::Value::Null
                    },
                    serde_json::Value::Number,
                )
            }
            ValueRef::String(StringRef::Variable { text, .. }) => {
                serde_json::Value::String(self.text(text))
            }
            ValueRef::String(StringRef::Encoded(encoded)) => {
                encoded.decode(&mut decoded)?;
                serde_json::Value::String(self.text(&decoded))
            }
            ValueRef::UnstructuredArray(encoded) => {
                encoded.decode(&mut decoded)?;
                serde_json::from_slice(&decoded).unwrap_or_else(|_| {
                    self.unverifiable = Some("array not representable as JSON");
                    serde_json::Value::Null
                })
            }
            ValueRef::EmptyObject => serde_json::Value::Object(serde_json::Map::new()),
            _ => anyhow::bail!("unknown value {value:?}"),
        };
        self.stack
            .last_mut()
            .context("an object is open")?
            .1
            .insert(key, json);
        Ok(())
    }
}

/// # Returns
///
/// The owned stored form of one column value, and why the reference cannot see it exactly.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`clp_s_table::EncodedTextRef::decode`]'s return values on failure.
/// * Forwards [`float_value`]'s return values on failure.
fn stored_value(
    value: ValueRef<'_>,
) -> anyhow::Result<(Option<StoredValue>, Option<&'static str>)> {
    let mut decoded = Vec::new();
    let mut problem = None;
    let stored = match value {
        ValueRef::Boolean(value) => Some(StoredValue::Scalar(OwnedScalar::Boolean(value))),
        ValueRef::Integer(value) => Some(StoredValue::Scalar(OwnedScalar::Integer(value))),
        ValueRef::Timestamp { epoch_ns, .. } => {
            Some(StoredValue::Scalar(OwnedScalar::Timestamp(epoch_ns)))
        }
        ValueRef::Float(float) => {
            Some(StoredValue::Scalar(OwnedScalar::Float(float_value(float)?)))
        }
        ValueRef::String(StringRef::Variable { id, .. }) => Some(StoredValue::Variable(id.get())),
        ValueRef::String(StringRef::Encoded(encoded)) => {
            encoded.decode(&mut decoded)?;
            Some(StoredValue::Encoded {
                logtype: encoded.logtype_id().get(),
                variables: encoded
                    .variables()?
                    .iter()
                    .map(|variable| variable.get())
                    .collect(),
                text: decoded,
            })
        }
        ValueRef::UnstructuredArray(encoded) => {
            encoded.decode(&mut decoded)?;
            let array = serde_json::from_slice(&decoded).unwrap_or_else(|_| {
                problem = Some("array not representable as JSON");
                serde_json::Value::Null
            });
            Some(StoredValue::Array(array))
        }
        _ => None,
    };
    Ok((stored, problem))
}

/// # Returns
///
/// The `f64` of a stored float.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`std::str::from_utf8`]'s return values on failure.
/// * Forwards [`str::parse`]'s return values on failure.
fn float_value(float: FloatRef<'_>) -> anyhow::Result<f64> {
    Ok(match float {
        FloatRef::Plain(value) | FloatRef::Formatted { value, .. } => value,
        FloatRef::Dictionary { text, .. } => std::str::from_utf8(text)?.parse()?,
    })
}

/// # Returns
///
/// The value of a leaf on the values below an unstructured array reached by `access`, with the
/// reference walker.
fn array_value(leaf: &Leaf, access: &ArrayAccess, array: &serde_json::Value) -> TruthValue {
    const ARRAY_KEY: &str = "\u{1}array";
    const PARENT_KEY: &str = "\u{1}parent";
    let key = |text: &str| PathToken::Key(text.as_bytes().into());
    let (record, column) = match access {
        ArrayAccess::Walk(tokens) => (
            serde_json::json!({ ARRAY_KEY: array }),
            path(std::iter::once(key(ARRAY_KEY)).chain(tokens.iter().cloned())),
        ),
        // Walking `parent.<tokens>` from `{parent: {array: ...}}` reaches what the tokens reach
        // below the array once the leading `*` consumed a level, plus the parent object itself
        // when the suffix is a lone `*`, which only `Exists` accepts and the array already
        // satisfies.
        ArrayAccess::BelowWildcard(tokens) => (
            serde_json::json!({ PARENT_KEY: { ARRAY_KEY: array } }),
            path(std::iter::once(key(PARENT_KEY)).chain(tokens.iter().cloned())),
        ),
        ArrayAccess::AnyLeaf => (serde_json::json!({ ARRAY_KEY: array }), ColumnRef::AnyLeaf),
    };
    let leaf = Leaf {
        column,
        ..leaf.clone()
    };
    leaf.evaluate(&JsonRecord::new(&record, None))
}

/// # Type Parameters
///
/// * `TokensType` - Iterates over the path tokens.
///
/// # Returns
///
/// The default-namespace path of `tokens`.
fn path<TokensType: Iterator<Item = PathToken>>(tokens: TokensType) -> ColumnRef {
    ColumnRef::Path(ColumnPattern {
        namespace: Namespace::Default,
        tokens: tokens.collect(),
    })
}

/// # Returns
///
/// Whether `variables` contains values passing every test, in order (C++
/// `SubQuery::matches_vars`).
fn matches_in_order(tests: &[clp_s_search::EncodedVariableTest], variables: &[i64]) -> bool {
    let mut tests = tests.iter().peekable();
    for variable in variables {
        if let Some(test) = tests.peek()
            && test.matches(*variable)
        {
            tests.next();
        }
    }
    tests.peek().is_none()
}

/// # Returns
///
/// True or False.
const fn truth(value: bool) -> TruthValue {
    if value {
        TruthValue::True
    } else {
        TruthValue::False
    }
}

/// # Returns
///
/// Range-index fields as JSON (binary values dropped, nested values decoded).
fn fields_to_json(fields: &MetadataFields) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    for (key, value) in fields.to_vec() {
        let json = match value {
            MetadataValue::Null => Some(serde_json::Value::Null),
            MetadataValue::Boolean(value) => Some(serde_json::Value::Bool(value)),
            MetadataValue::Integer(value) => Some(value.into()),
            MetadataValue::UnsignedInteger(value) => Some(value.into()),
            MetadataValue::Float(value) => {
                serde_json::Number::from_f64(value).map(serde_json::Value::Number)
            }
            MetadataValue::String(text) => Some(serde_json::Value::String(text.to_owned())),
            _ => None,
        };
        if let Some(json) = json {
            object.insert(key.to_owned(), json);
        }
    }
    serde_json::Value::Object(object)
}

/// One query of the suite, with its options.
pub struct SuiteQuery {
    pub id: String,
    pub target: String,
    pub expected_rust_count: Option<u64>,
    pub kql: String,
    pub is_case_sensitive: bool,
    pub tge: Option<i64>,
    pub tle: Option<i64>,
    pub projection: Vec<String>,
}

/// # Returns
///
/// The C++ `clp-s s` stdout record count of every suite query with a stdout oracle, by id.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`std::fs::read_to_string`]'s return values on failure.
/// * Forwards [`str::parse`]'s return values on failure.
pub fn oracle_counts() -> anyhow::Result<std::collections::HashMap<String, u64>> {
    let path = prototype_dir().join("tests/queries/oracles/manifest.toml");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let table: toml::Table = text.parse()?;
    Ok(table
        .get("oracle")
        .and_then(toml::Value::as_array)
        .map(|oracles| {
            oracles
                .iter()
                .filter_map(toml::Value::as_table)
                .filter(|oracle| {
                    oracle.get("output").and_then(toml::Value::as_str) == Some("stdout")
                })
                .filter_map(|oracle| {
                    let id = oracle.get("id")?.as_str()?.to_owned();
                    let count = u64::try_from(oracle.get("count")?.as_integer()?).ok()?;
                    Some((id, count))
                })
                .collect()
        })
        .unwrap_or_default())
}

/// # Returns
///
/// Every query of `tests/queries/suite.toml`.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if a query lacks its id or KQL.
/// * Forwards [`std::fs::read_to_string`]'s return values on failure.
/// * Forwards [`str::parse`]'s return values on failure.
pub fn suite_queries() -> anyhow::Result<Vec<SuiteQuery>> {
    let path = prototype_dir().join("tests/queries/suite.toml");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let table: toml::Table = text.parse()?;
    let queries = table
        .get("query")
        .and_then(toml::Value::as_array)
        .context("the suite has queries")?;
    queries
        .iter()
        .map(|query| {
            let query = query.as_table().context("a query is a table")?;
            let string = |key: &str| query.get(key).and_then(toml::Value::as_str);
            Ok(SuiteQuery {
                id: string("id").context("a query has an id")?.to_owned(),
                target: string("target").unwrap_or_default().to_owned(),
                expected_rust_count: query
                    .get("expected_rust_count")
                    .and_then(toml::Value::as_integer)
                    .and_then(|count| u64::try_from(count).ok()),
                kql: string("kql").context("a query has KQL")?.to_owned(),
                is_case_sensitive: !query
                    .get("ignore_case")
                    .and_then(toml::Value::as_bool)
                    .unwrap_or(false),
                tge: query.get("tge").and_then(toml::Value::as_integer),
                tle: query.get("tle").and_then(toml::Value::as_integer),
                projection: query
                    .get("projection")
                    .and_then(toml::Value::as_array)
                    .map(|columns| {
                        columns
                            .iter()
                            .filter_map(|column| column.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}
