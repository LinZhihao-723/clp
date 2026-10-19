# Rust clp-s reader: API overview

Hosts read archives through one crate, `clp-s-reader`, which re-exports a small facade: open an
`Archive`, describe what you want in a `ScanRequest`, then either pull batches from a `Scan` or push
a multi-archive `SearchSession` into a `Sink`. This document lists that surface for review before the
API is frozen. The design behind it is in [design.md](design.md): Part 1 §5 is the target API and
Part 2 the prototype's contracts.

## Overview

- **Scope of this round (M0-M3):** single-file archives, format 0.5.x, from a local file, memory or
  S3; unordered decompression and search; outputs to JSON lines, msgpack file/network and the
  results cache with session-wide top-k.
- **Not yet in the API:** multi-file archives, structured arrays and pre-0.5.0 archives (all
  rejected with `UnsupportedArchive`), IR features, ordered decompression, typed columns and Arrow
  export, the worker pool. The enums that will grow are `#[non_exhaustive]`.
- **Threading:** everything runs on the calling thread (`Executor::CurrentThread`). `Archive`,
  `ReaderContext`, `ScanRequest` and `Query` are `Send + Sync` and shared through `Arc`; `Scan` is
  `Send + 'static`, so a binding can hold it.
- **Two delivery shapes, one engine:** the pull cursor and the push session run the same planner and
  per-archive pipeline; the CLI, the results-cache path and the Spider adapter are built on the push
  session.

## Quick start

Three calls cover the common cases: decompress one archive, search one archive with a pull cursor,
and search many archives into a sink. All examples use only names re-exported by `clp-s-reader`
(sinks come from `clp-s-sinks`).

Decompress an archive to stdout (pull):

```rust
let context = ReaderContext::builder().build()?; // Arc<ReaderContext>
let archive = Archive::open(
    Arc::clone(&context),
    ArchiveId::from("a1"),
    ArchiveLocation::File(path),
)?;
let request = Arc::new(
    ScanRequest::builder()
        .outputs(OutputRequirements::json())
        .build()?,
);
let mut scan = archive.scan(request, CancellationToken::new())?;
let mut stdout = std::io::stdout().lock();
while let Some(batch) = scan.next_batch()? {
    if let Some(json) = batch.json() {
        stdout.write_all(json.as_jsonl())?;
    }
}
```

Search one archive with a time range and timestamps (pull):

```rust
let query = Query::from_kql("level: ERROR AND latency_ms > 500", &QueryOptions::default())?;
let request = ScanRequest::builder()
    .query(query)
    .time_bounds(TimeBounds::from_epoch_millis(Some(1_735_689_600_000), None))
    .outputs(OutputRequirements::json().with_timestamps())
    .build()?;
let mut scan = archive.scan(Arc::new(request), CancellationToken::new())?;
while scan.advance()? {
    let Some(batch) = scan.current() else {
        break;
    };
    let json = batch.json().expect("JSON was requested");
    let timestamps = batch.timestamps_ns().expect("timestamps were requested");
    for record in 0..batch.num_records() {
        handle(json.record(record), timestamps[record]);
    }
}
println!("{:?}", scan.stats());
```

Search many archives into a sink (push):

```rust
let sink = Arc::new(JsonLinesSink::new(std::io::stdout()));
let report = SearchSession::builder(context, Arc::new(request))
    .archive(ArchiveId::from("a1"), ArchiveLocation::File(first_path))
    .archive(ArchiveId::from("a2"), ArchiveLocation::File(second_path))
    .skip_failed_archives(true)
    .build()?
    .run(Arc::clone(&sink))?;
println!("{} records; {:?}", report.output, report.archive_summaries);
```

The session merges the sink's `requirements()` into the request, so the request above does not
need to ask for JSON itself.

## Context and opening archives

A host builds one `ReaderContext` per process (buffer pool, memory budget, execution config) and
opens each archive against it; `Archive::open` reads the header, metadata and schema layer only,
and rejects unsupported archives before any scan starts.

```rust
pub struct ReaderContext { /* executor, buffer pool, memory budget, config */ }

impl ReaderContext {
    pub fn builder() -> ReaderContextBuilder;
    pub const fn executor(&self) -> Executor;
    pub const fn config(&self) -> &ExecutionConfig;
    pub const fn memory_budget(&self) -> &Arc<MemoryBudget>;
}

impl ReaderContextBuilder {
    pub const fn memory_budget_size(self, limit_size: Option<u64>) -> Self; // None = unlimited
    pub const fn config(self, config: ExecutionConfig) -> Self;
    pub fn build(self) -> Result<Arc<ReaderContext>, ReadError>;
}

#[non_exhaustive]
pub enum Executor {
    #[default]
    CurrentThread, // the only variant this round
}

#[non_exhaustive]
pub struct ExecutionConfig {
    pub batch_num_rows: NonZeroU32,   // rows per scan window, default 4,096
    pub max_rendered_batch_size: u64, // JSON bytes before a window splits, default 256 KiB
    pub stream_chunk_size: u64,       // streaming decode chunk for local files, default 1 MiB
    pub open_options: OpenOptions,    // reader limits + 64 KiB prefix fetch
}
```

Where an archive lives, and the open call:

```rust
#[non_exhaustive]
pub enum ArchiveLocation {
    File(PathBuf),               // a directory (multi-file archive) is rejected: UnsupportedArchive
    Memory(SharedBytes),
    Source(Arc<dyn ByteSource>), // host-provided, e.g. clp_s_reader::s3::S3Source
}

pub struct ArchiveId(Arc<str>); // From<&str>, From<String>, as_str()

impl Archive {
    pub fn open(
        context: Arc<ReaderContext>,
        archive_id: ArchiveId,
        location: ArchiveLocation,
    ) -> Result<Self, ReadError>;
    pub fn info(&self) -> &ArchiveInfo;
    pub fn tables(&self) -> &[TableInfo]; // every ERT, no decompression
    pub fn scan(
        &self,
        request: Arc<ScanRequest>,
        cancellation_token: CancellationToken,
    ) -> Result<Scan, ReadError>;
}

#[non_exhaustive]
pub struct ArchiveInfo {
    pub archive_id: ArchiveId,
    pub version: FormatVersion,
    pub num_log_events: u64,                          // from the table metadata
    pub has_log_order: bool,
    pub timestamp_range_ms: Option<TimestampRangeMs>, // { begin, end }, inclusive
    pub file_size: u64,
}
```

Host byte sources implement one trait; S3 is the built-in example
(`S3Source::new(client, runtime, location, config)`, one ranged GET per coalesced range):

```rust
pub trait ByteSource: Send + Sync {
    fn capabilities(&self) -> SourceCapabilities; // { is_resident, is_remote }
    fn size(&self) -> Result<u64, std::io::Error>;
    fn read_exact_at(
        &self,
        offset: u64,
        buffer: &mut [u8],
        context: &ReadContext<'_>, // context.check() reports cancellation
    ) -> Result<(), std::io::Error>;
    fn slice(&self, range: ByteRange) -> Option<SharedBytes>; // default None
}
```

## Requests

A `ScanRequest` is immutable and shared by every archive of a session: the query, time bounds,
projection, what to materialize, and the result order. `build()` folds the time bounds into the
query, so a request whose bounds are disjoint from an archive's range prunes that archive before any
I/O beyond the metadata.

```rust
impl Query {
    pub fn from_kql(text: &str, options: &QueryOptions) -> Result<Self, QueryError>;
    pub fn from_expr(expr: &LogicalExpr, options: &QueryOptions) -> Result<Self, QueryError>;
    pub fn match_all() -> Self; // also what an empty request uses
}

pub struct QueryOptions {
    pub is_case_sensitive: bool, // default true; -i in the CLI
    pub limits: QueryLimits,     // max_depth 1,024, max_leaves 65,536
}

pub struct TimeBounds { /* inclusive, epoch ns */ }

impl TimeBounds {
    pub const fn from_epoch_millis(begin_epoch_ms: Option<i64>, end_epoch_ms: Option<i64>) -> Self;
}

impl ScanRequestBuilder {
    pub fn query(self, query: Query) -> Self;
    pub fn time_bounds(self, time_bounds: TimeBounds) -> Self;
    pub fn projection(self, projection: RecordProjection) -> Self;
    pub fn outputs(self, outputs: OutputRequirements) -> Self;
    pub fn order(self, order: ResultOrder) -> Self;
    pub fn json_options(self, options: JsonOptions) -> Self;
    pub fn build(self) -> Result<ScanRequest, ReadError>;
}
```

What gets materialized, and in which order:

```rust
#[non_exhaustive]
pub struct OutputRequirements {
    pub is_json_needed: bool,
    pub is_timestamp_needed: bool,
    pub is_log_event_idx_needed: bool,
    pub required_order: Option<ResultOrder>, // a conflicting request order fails the session build
}
// Constructors: none(), json(), .with_timestamps(), .with_log_event_indices(),
// .with_required_order(order), .merge(other) -> Result<Self, ReadError>

#[non_exhaustive]
pub enum ResultOrder {
    #[default]
    Any,                               // unspecified (table order, then rows, on one thread)
    Scan,                              // archive, then table-directory, then row order (C++ order)
    LatestFirst { limit: NonZeroU32 }, // session-wide top-k by LatestFirstKey
}

pub enum RecordProjection {
    #[default]
    Full,
    Columns(Vec<ColumnPattern>), // ColumnPattern::parse("a.b"); C++ projection rules apply
}

pub struct JsonOptions {
    pub should_emulate_cpp_plain_floats: bool, // differential tests only; default prints shortest round-trip
}
```

| Need | Request |
|---|---|
| Decompress an archive | default query (`match_all`), `OutputRequirements::json()` |
| Count matches only | `OutputRequirements::none()`; read `num_records()` per batch |
| Latest N results with timestamps | `order(ResultOrder::LatestFirst { limit })`, `json().with_timestamps()` |
| A few fields per record | `projection(RecordProjection::Columns(vec![...]))` |
| Case-insensitive search | `QueryOptions::default().with_case_sensitivity(false)` |

## Pull iteration

`Scan` is a lending cursor: `advance` moves to the next batch with at least one selected record,
and `current` borrows that batch until the next `advance`. The scan owns its decode state (no borrow
of the `Archive`), so it is `Send + 'static` and a binding can store it in a handle.

```rust
impl Scan {
    pub fn advance(&mut self) -> Result<bool, ReadError>; // false at the end
    pub fn current(&self) -> Option<RecordBatch<'_>>;     // valid until the next advance
    pub fn next_batch(&mut self) -> Result<Option<RecordBatch<'_>>, ReadError>; // advance + current
    pub const fn stats(&self) -> ScanStats;
    pub const fn cancellation_token(&self) -> &CancellationToken;
}
```

Every accessor on a batch is indexed by the selected-record ordinal `0..num_records()`; per-record
columns are `None` unless the request asked for them.

```rust
impl<'batch> RecordBatch<'batch> {
    pub const fn num_records(&self) -> usize;
    pub fn archive_id(&self, record: usize) -> &ArchiveId;          // panics if out of range
    pub const fn record_ids(&self) -> &[RecordId];                   // log_event_idx, or table-order id
    pub const fn timestamps_ns(&self) -> Option<&[Option<i64>]>;     // is_timestamp_needed
    pub const fn log_event_indices(&self) -> Option<&[Option<i64>]>; // is_log_event_idx_needed
    pub const fn json(&self) -> Option<&JsonRecordBuffer>;           // is_json_needed
    pub const fn rows(&self) -> &BatchRows<'batch>;
}

#[non_exhaustive]
pub enum BatchRows<'batch> {
    Table { batch: TableBatch<'batch>, rows: &'batch [u32] }, // one window of one ERT
    Gathered(&'batch OwnedBatch),                              // the final top-k batch
}

impl JsonRecordBuffer {
    pub const fn len(&self) -> usize;
    pub fn record(&self, index: usize) -> Option<&[u8]>; // one record, no trailing newline
    pub fn as_jsonl(&self) -> &[u8];                     // all records, newline-terminated
    pub const fn byte_size(&self) -> u64;
}
```

- **Batch shape:** a borrowed batch is at most one 4,096-row window of one table, or several
  batches when its rendered JSON exceeds 256 KiB. Batches with zero selected records are never
  returned.
- **Top-k:** under `ResultOrder::LatestFirst`, a scan returns one gathered batch with the archive's
  top `limit` records, sorted by `LatestFirstKey { timestamp_ms, archive_id, record_id }`.
- **Cancellation:** cancel from any thread through `scan.cancellation_token().clone()`; the next
  `advance` returns a `Cancelled` error. Dropping a `Scan` releases its buffers.
- **Thread-safety (asserted at compile time):** `ReaderContext`, `Archive`, `ScanRequest` and
  `ReadError` are `Send + Sync + 'static`; `Scan` is `Send + 'static`.

## Push sessions and sinks

A `SearchSession` runs one request over many archives with one sink lifecycle; the sink follows
tantivy's `Collector` shape: a session-level `Sink` opens one `PartialSink` per partition (one per
archive, plus a final top-k partition under `LatestFirst`) and merges their outputs.

```rust
impl SearchSession {
    pub fn builder(context: Arc<ReaderContext>, request: Arc<ScanRequest>) -> SearchSessionBuilder;
    pub fn run<SinkType: Sink>(
        self,
        sink: Arc<SinkType>,
    ) -> Result<SessionReport<SinkType::Output>, ReadError>;
}

impl SearchSessionBuilder {
    pub fn archive(self, archive_id: ArchiveId, location: ArchiveLocation) -> Self; // repeat per archive
    pub fn cancellation_token(self, cancellation_token: CancellationToken) -> Self;
    pub const fn skip_failed_archives(self, is_failed_archive_skipped: bool) -> Self;
    pub fn build(self) -> Result<SearchSession, ReadError>; // merges sink requirements, checks order conflicts
}

pub struct SessionReport<SinkOutputType> {
    pub output: SinkOutputType,
    pub archive_summaries: Vec<ArchiveSummary>, // archive order
}

pub struct ArchiveSummary {
    pub archive_id: ArchiveId,
    pub termination_stage: Option<TerminationStage>, // None if the archive failed to open
    pub prune_reason: Option<PruneReason>,
    pub stats: ScanStats,
    pub error: Option<ReadError>, // set when failed archives are skipped
}
```

The sink traits:

```rust
pub trait Sink: Send + Sync + 'static {
    type Partial: PartialSink;
    type Output: Default + Send + 'static;

    fn requirements(&self) -> OutputRequirements;
    fn merge_order(&self) -> MergeOrder; // Commutative | Scan (#[non_exhaustive])
    fn open_partial(&self, partition: &PartitionInfo) -> Result<Self::Partial, SinkError>;
    fn merge(
        &self,
        output: &mut Self::Output,
        partial: <Self::Partial as PartialSink>::Output,
    ) -> Result<SinkControl, SinkError>;
}

pub trait PartialSink: Send + 'static {
    type Output: Send + 'static;

    fn consume(&mut self, batch: &RecordBatch<'_>) -> Result<SinkControl, SinkError>;
    fn finish(self, summary: &PartitionSummary) -> Result<Self::Output, SinkError>; // also for pruned archives
}

pub enum SinkControl { Continue, StopArchive, StopSession } // stops still end with Ok
pub struct PartitionInfo { pub index: u32, pub kind: PartitionKind, pub order: ResultOrder }
pub enum PartitionKind { Archive(ArchiveId), TopK }
```

Built-in sinks and the C++ output handler each replaces:

| Sink (crate) | Replaces C++ handler | Requirements | Merge order | Output |
|---|---|---|---|---|
| `JsonLinesSink<WriterType>` (`clp-s-sinks`) | `StandardOutputHandler` | JSON | `Scan` | records written (`u64`) |
| `MsgpackResultSink<WriterType>` (`clp-s-sinks`) | `FileOutputHandler`, `NetworkOutputHandler` | JSON, timestamp, `log_event_idx` | `Commutative` | records written (`u64`) |
| `ResultsCacheSink` (`clp-s-results-cache`) | `ResultsCacheOutputHandler` | JSON, timestamp | `Commutative` | documents inserted (`u64`) |

`MsgpackResultSink` writes the C++ tuple `(timestamp, message, "", archive_id, log_event_idx)` and
is opened once per session; `ResultsCacheSink::new(runtime, database, collection, dataset, config)`
takes a host-built MongoDB database handle and pairs with `ResultOrder::LatestFirst` for top-k.

## Errors, stats and observability

Every public call returns one error type, `ReadError` (boxed payloads, at most 32 bytes), and
callers branch on `ReadError::kind()` rather than on the per-layer variants. "No matches" is never
an error: a pruned archive yields a scan with no batch and a `PruneReason` in its summary.

```rust
#[non_exhaustive]
pub enum ReadError {
    InvalidArgument(Box<str>),
    Query(Box<QueryError>),
    Storage(Box<StorageError>),
    Layout(Box<LayoutError>),
    Table(Box<TableError>),
    Json(Box<JsonError>),
    Plan(Box<PlanError>),
    Eval(Box<EvalError>),
    Resource(Box<ResourceError>),
    Sink(Box<SinkError>),
    Internal(Box<str>),
    InArchive { archive_id: ArchiveId, source: Box<Self> },
}

impl ReadError {
    pub fn kind(&self) -> ErrorKind;
    pub fn in_archive(self, archive_id: ArchiveId) -> Self;
}

pub enum SinkError {
    Io(std::io::Error),
    DownstreamClosed,
    Database(Box<dyn std::error::Error + Send + Sync>),
    Encode(Box<str>),
    Read(Box<ReadError>),
    Custom(Box<dyn std::error::Error + Send + Sync>),
}
```

| `ErrorKind` | Meaning | `clp-s-rs` exit code |
|---|---|---|
| `InvalidArgument` | Bad argument or configuration | 1 |
| `InvalidQuery` | Query text or expression is invalid | 1 |
| `CorruptArchive` | Archive bytes are corrupt | 2 |
| `UnsupportedArchive` | Multi-file, structured arrays, version outside [0.5.0, 0.5.x] | 2 |
| `Io` | Reading the archive failed | 3 |
| `Sink` | A sink or downstream consumer failed | 4 |
| `ResourceExhausted` | A limit or the memory budget was exceeded, or an allocation failed | 5 |
| `Cancelled` | The caller cancelled | 130 |
| `Internal` | An internal invariant was violated | 70 |

Statistics are returned values, never global state:

```rust
pub struct ScanStats {
    pub num_tables_planned: u64,
    pub num_tables_scanned: u64,
    pub num_rows_scanned: u64,
    pub num_records_selected: u64,
    pub num_records_rendered: u64,
    pub num_streams_decoded: u64,
    pub num_decoded_bytes: u64,
    pub num_bytes_read: u64,
}
// TerminationStage: RangeIndexMatching | TimeRangeMatching | TimeRangeMatchingAfterColumnResolution
//   | SchemaMatching | DictionarySearch | ErtScan | LogicallyFalse | StoppedBySink
// PruneReason: LogicallyFalse | TimeRangeDisjoint | NoTimestampColumn | RangeIndexMismatch
//   | NoCandidateTable | DictionaryMiss
```

## Design notes and open questions

The surface is small and consistent, but documenting it turned up a few types that leak from
internal crates and two APIs of the target design ([design.md](design.md) Part 1 §5) that this
round does not have yet.

**Deviations from the target API of design.md Part 1 §5 (by decision this round)**

- S3 is `ArchiveLocation::Source(Arc::new(S3Source::new(..)?))`, re-exported as
  `clp_s_reader::s3::*` behind feature `s3`, instead of an `ArchiveLocation::S3` variant.
- `Archive::open` loads the schema layer eagerly, so unsupported archives fail at open rather than
  on first scan.
- No `Archive::plan` / EXPLAIN, no multi-archive pull cursor (`SessionCursor`), no typed columns,
  record views or Arrow export. A pull `Scan` under `LatestFirst` gives the archive's own top-k;
  session-wide top-k exists only in the push session.
- `ErrorKind` lives in `clp-s-resources` so every layer maps its own errors.

**Rough edges found while documenting**

- Types in public signatures that `clp-s-reader` does not re-export: `OpenOptions` and
  `ReaderLimits` (field of `ExecutionConfig`), `TableBatch` (inside `BatchRows::Table`), `ByteRange`
  (in `ByteSource::slice`), and the per-layer error payloads of `ReadError` (`StorageError`,
  `TableError`, `PlanError`, ...). Hosts cannot name them without depending on internal crates.
- `ExecutionConfig` is `#[non_exhaustive]` with public fields and no builder, so hosts must start
  from `default()` and assign fields.
- `BatchRows::Table` exposes window-space rows and a physical `TableBatch`, although no public
  accessor consumes them this round.
- Per-record columns are `Option<&[Option<i64>]>` (requested? then per-record presence). An
  Arrow-style values + validity layout would avoid the inner `Option` when typed export lands.
- `OutputRequirements` is three `bool`s plus an order; the style guide prefers bools for binary
  choices, but the set will grow (typed columns, record views).

**Questions for review**

1. Should the facade re-export (or wrap) every type that appears in a public signature, and should
   `ReadError`'s variants become opaque so the per-layer errors stay internal?
2. Should `BatchRows` stay public now, or be hidden until typed columns give it a consumer?
3. Is `Arc<ScanRequest>` + `CancellationToken` by value the right ownership for `Archive::scan`, or
   should the token be optional (default to a fresh one)?
4. Should the next round add the multi-archive pull cursor and session-wide top-k in pull form,
   which the Python wheel and the C ABI will need?
5. Is the `Sink: Send + Sync + 'static` + `run(Arc<Sink>)` shape acceptable for hosts, or do we want
   a borrowed-sink `run_scoped` before the API is frozen?
