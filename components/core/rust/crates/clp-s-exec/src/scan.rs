//! The pull cursor.

use std::ops::Range;
use std::sync::Arc;

use clp_s_format::DictionaryNeeds;
use clp_s_format::SharedBytes;
use clp_s_format::StreamDecoder;
use clp_s_format::StreamRead;
use clp_s_format::TableBytes;
use clp_s_format::TableInfo;
use clp_s_format::TableOrdinal;
use clp_s_format::TableSet;
use clp_s_io::ArchiveStorage;
use clp_s_io::StreamReadOptions;
use clp_s_io::StreamReader;
use clp_s_json::JsonRecordBuffer;
use clp_s_json::JsonTemplate;
use clp_s_query::NormalExpr;
use clp_s_resources::CancellationToken;
use clp_s_resources::PooledBuffer;
use clp_s_search::ArchiveSearchPlan;
use clp_s_search::CompiledTableFilter;
use clp_s_search::EvaluationScratch;
use clp_s_search::PlanStep;
use clp_s_search::PruneReason;
use clp_s_search::PruneReport;
use clp_s_search::SearchPlanner;
use clp_s_search::TableFilterPlan;
use clp_s_search::TerminationStage;
use clp_s_table::ColumnDemand;
use clp_s_table::ColumnSet;
use clp_s_table::LeadingColumnsSize;
use clp_s_table::RecordId;
use clp_s_table::ResolvedProjection;
use clp_s_table::Table;
use clp_s_table::TableError;
use clp_s_table::TableLayout;
use clp_s_table::TableScanState;
use clp_s_table::TableScanner;

use crate::Archive;
use crate::LatestFirstKey;
use crate::OwnedBatch;
use crate::ReadError;
use crate::RecordBatch;
use crate::ResultOrder;
use crate::ScanRequest;
use crate::ScanStats;
use crate::batch::RecordColumns;
use crate::topk::TopK;
use crate::topk::TopKRecord;

/// Pull cursor over one archive plan; owns what it reads, so it can be moved across threads.
///
/// Batches are delivered in table-directory order, then row order; under
/// [`ResultOrder::LatestFirst`] the archive's top-k is delivered as one gathered batch sorted by
/// [`LatestFirstKey`]. A batch with no selected record is never delivered. Cancellation is checked
/// before every window.
pub struct Scan {
    archive: Archive,
    request: Arc<ScanRequest>,
    cancellation_token: CancellationToken,
    termination_stage: TerminationStage,
    prune_reason: Option<PruneReason>,
    projection: ResolvedProjection,

    /// The candidate tables, sorted by ordinal.
    candidates: Vec<Candidate>,

    reads: std::vec::IntoIter<StreamRead>,
    stream: Option<StreamCursor>,
    table: Option<TableCursor>,

    /// Whether the selected rows of the scanner's current window are being delivered.
    is_window_active: bool,

    /// The position in `buffers.selected_rows` of the first row not delivered yet.
    next_selected: usize,

    /// Whether [`Self::current`] has a batch.
    has_current: bool,

    /// The archive's top-k winners under [`ResultOrder::LatestFirst`], once collected.
    gathered: Option<OwnedBatch>,

    /// Whether the top-k winners were collected (they are delivered at most once).
    is_top_k_collected: bool,

    scanner: TableScanner,
    decoder: StreamDecoder,
    scratch: EvaluationScratch,
    buffers: BatchBuffers,
    stats: ScanStats,
    span: tracing::Span,
}

impl Scan {
    /// Moves to the next batch with at least one selected record.
    ///
    /// # Returns
    ///
    /// Whether a batch is available through [`Self::current`], on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::advance_to_batch`]'s return values on failure.
    /// * Forwards [`Self::advance_to_top_k`]'s return values on failure.
    pub fn advance(&mut self) -> Result<bool, ReadError> {
        let span = self.span.clone();
        let _entered = span.enter();
        self.has_current = false;
        let result = if let ResultOrder::LatestFirst { limit } = self.request.order() {
            self.advance_to_top_k(limit)
        } else {
            self.advance_to_batch()
        };
        if result.is_err() {
            self.finish();
        }
        result
    }

    /// # Returns
    ///
    /// The batch [`Self::advance`] stopped at, until the next call to [`Self::advance`].
    #[must_use]
    pub fn current(&self) -> Option<RecordBatch<'_>> {
        if !self.has_current {
            return None;
        }
        let outputs = self.request.outputs();
        if let Some(gathered) = &self.gathered {
            return Some(RecordBatch::from_gathered(gathered, outputs));
        }
        let table = self.table.as_ref()?;
        let columns = RecordColumns {
            record_ids: &self.buffers.record_ids,
            timestamps_ns: outputs
                .is_timestamp_needed
                .then_some(self.buffers.timestamps_ns.as_slice()),
            log_event_indices: outputs
                .is_log_event_idx_needed
                .then_some(self.buffers.log_event_indices.as_slice()),
            json: table.template.is_some().then_some(&self.buffers.json),
        };
        Some(RecordBatch::from_table(
            self.scanner.batch(&table.state),
            &self.buffers.selected_rows[self.buffers.delivered_rows.clone()],
            &self.archive.info().archive_id,
            columns,
        ))
    }

    /// Advances and returns the next batch.
    ///
    /// # Returns
    ///
    /// The next batch, or `None` at the end of the scan, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::advance`]'s return values on failure.
    pub fn next_batch(&mut self) -> Result<Option<RecordBatch<'_>>, ReadError> {
        Ok(if self.advance()? {
            self.current()
        } else {
            None
        })
    }

    /// # Returns
    ///
    /// The statistics of the scan so far.
    #[must_use]
    pub const fn stats(&self) -> ScanStats {
        self.stats
    }

    /// # Returns
    ///
    /// The scan's cancellation token; cancel it from any thread to stop the scan.
    #[must_use]
    pub const fn cancellation_token(&self) -> &CancellationToken {
        &self.cancellation_token
    }

    /// Factory function.
    ///
    /// Plans `request` against `archive` (with `planner` if given, else with a planner of its
    /// own), loads every dictionary the scan reads and plans the stream reads.
    ///
    /// # Returns
    ///
    /// The cursor on success; a pruned archive yields a cursor with no batch.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::plan`]'s return values on failure.
    /// * Forwards [`Candidate::compile`]'s return values on failure.
    /// * Forwards [`Self::load_dictionaries`]'s return values on failure.
    /// * Forwards [`StreamDecoder::new`]'s return values on failure.
    pub(crate) fn start(
        archive: Archive,
        request: Arc<ScanRequest>,
        cancellation_token: CancellationToken,
        planner: Option<&SearchPlanner>,
    ) -> Result<Self, ReadError> {
        let scan_span = tracing::debug_span!(
            "clp_s.scan",
            archive_id = % archive.info().archive_id
        );
        let config = *archive.context().config();
        let mut scan = Self {
            archive,
            request,
            cancellation_token,
            termination_stage: TerminationStage::ErtScan,
            prune_reason: None,
            projection: ResolvedProjection::Full,
            candidates: Vec::new(),
            reads: Vec::new().into_iter(),
            stream: None,
            table: None,
            is_window_active: false,
            next_selected: 0,
            has_current: false,
            gathered: None,
            is_top_k_collected: false,
            scanner: TableScanner::new(config.batch_num_rows),
            decoder: StreamDecoder::new().map_err(TableError::from)?,
            scratch: EvaluationScratch::default(),
            buffers: BatchBuffers::default(),
            stats: ScanStats::default(),
            span: scan_span,
        };
        let plan = {
            let _entered = tracing::debug_span!(parent: &scan.span, "clp_s.plan").entered();
            scan.plan(planner)?
        };
        let plan = match plan {
            Planned::Pruned(report) => {
                scan.termination_stage = report.stage;
                scan.prune_reason = Some(report.reason);
                return Ok(scan);
            }
            Planned::FullScan => None,
            Planned::Search(plan) => Some(plan),
        };

        let mut dictionary_needs = DictionaryNeeds::default();
        let tables: Vec<(TableOrdinal, Option<usize>)> = if let Some(plan) = &plan {
            scan.projection = plan.projection().clone();
            dictionary_needs = plan.filter_dictionary_needs();
            plan.tables()
                .iter()
                .enumerate()
                .map(|(index, table)| (table.table, Some(index)))
                .collect()
        } else {
            scan.archive
                .tables()
                .iter()
                .map(|info| (info.ordinal, None))
                .collect()
        };
        let num_tables = u32::try_from(scan.archive.tables().len())
            .map_err(|_| ReadError::Internal("the table directory exceeds u32 tables".into()))?;
        let mut table_set = TableSet::empty(num_tables);
        for (ordinal, plan_table) in tables {
            let filter = plan
                .as_deref()
                .zip(plan_table)
                .map(|(plan, index)| (plan, &plan.tables()[index].filter));
            let candidate = Candidate::compile(&scan, ordinal, filter)?;
            dictionary_needs = dictionary_needs.union(candidate.dictionary_needs);
            table_set.insert(ordinal);
            scan.candidates.push(candidate);
        }
        scan.candidates.sort_by_key(|candidate| candidate.ordinal);
        scan.stats.num_tables_planned = scan.candidates.len() as u64;
        if scan.candidates.is_empty() {
            return Ok(scan);
        }
        scan.load_dictionaries(dictionary_needs)?;
        scan.reads = scan
            .archive
            .storage()
            .index()
            .plan_reads(&table_set, dictionary_needs)
            .streams
            .into_iter();
        Ok(scan)
    }

    /// Offers every selected record of the scan to `top_k`, rendering only the records it keeps.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::offer_window_rows`]'s return values on failure.
    /// * Forwards [`Self::open_window`]'s return values on failure.
    /// * Forwards [`Self::open_table`]'s return values on failure.
    /// * Forwards [`Self::open_stream`]'s return values on failure.
    pub(crate) fn collect_top_k(&mut self, top_k: &mut TopK) -> Result<(), ReadError> {
        let span = self.span.clone();
        let _entered = span.enter();
        let result = self.offer_every_record(top_k);
        if result.is_err() {
            self.finish();
        }
        result
    }

    /// # Returns
    ///
    /// The stage at which the scan of the archive ends (or ended).
    pub(crate) const fn termination_stage(&self) -> TerminationStage {
        self.termination_stage
    }

    /// # Returns
    ///
    /// Why planning proved the archive empty, if it did.
    pub(crate) const fn prune_reason(&self) -> Option<PruneReason> {
        self.prune_reason
    }

    /// Plans the request: a query that folds to a constant with full projection needs no
    /// planner; anything else goes through stages A-D.
    ///
    /// # Returns
    ///
    /// The planning outcome on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`ScanRequest::search_planner`]'s return values on failure.
    /// * Forwards [`clp_s_search::ArchivePlanner::advance`]'s return values on failure.
    /// * Forwards [`Self::load_dictionaries`]'s return values on failure.
    fn plan(&mut self, planner: Option<&SearchPlanner>) -> Result<Planned, ReadError> {
        if !self.request.requires_search_planner() {
            return Ok(match self.request.query().root() {
                NormalExpr::MatchNone => Planned::Pruned(PruneReport {
                    stage: TerminationStage::LogicallyFalse,
                    reason: PruneReason::LogicallyFalse,
                }),
                _ => Planned::FullScan,
            });
        }
        let own_planner;
        let planner = if let Some(planner) = planner {
            planner
        } else {
            own_planner = self.request.search_planner()?;
            &own_planner
        };
        if let Some(report) = planner.prune_on_metadata(self.archive.storage().index().metadata()) {
            return Ok(Planned::Pruned(report));
        }
        let mut archive_planner = planner.plan_archive(Arc::clone(self.archive.archive_tables()));
        loop {
            match archive_planner.advance()? {
                PlanStep::NeedsDictionaries(needs) => self.load_dictionaries(needs)?,
                PlanStep::Pruned(report) => return Ok(Planned::Pruned(report)),
                PlanStep::Planned(plan) => return Ok(Planned::Search(plan)),
            }
        }
    }

    /// Loads the dictionaries of `needs` into the archive, counting the bytes read.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`clp_s_io::ArchiveStorage::load_dictionaries`]'s return values on failure.
    fn load_dictionaries(&mut self, needs: DictionaryNeeds) -> Result<(), ReadError> {
        let storage = self.archive.storage();
        let num_bytes_read_before = storage.num_bytes_read();
        let result = storage.load_dictionaries(needs, &self.cancellation_token);
        self.stats.num_bytes_read += storage
            .num_bytes_read()
            .saturating_sub(num_bytes_read_before);
        Ok(result?)
    }

    /// Moves to the next batch, opening windows, tables and streams as needed.
    ///
    /// # Returns
    ///
    /// Whether a batch is available on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::deliver_window_rows`]'s return values on failure.
    /// * Forwards [`Self::open_window`]'s return values on failure.
    /// * Forwards [`Self::open_table`]'s return values on failure.
    /// * Forwards [`Self::open_stream`]'s return values on failure.
    fn advance_to_batch(&mut self) -> Result<bool, ReadError> {
        loop {
            if self.deliver_window_rows()? {
                self.has_current = true;
                return Ok(true);
            }
            if self.open_window()? || self.open_table()? || self.open_stream()? {
                continue;
            }
            return Ok(false);
        }
    }

    /// Collects the archive's top-`limit` winners on the first call and makes them the current
    /// batch.
    ///
    /// # Returns
    ///
    /// Whether the winners are available through [`Self::current`] on success; `false` once they
    /// were delivered or if no record matched.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::offer_every_record`]'s return values on failure.
    /// * Forwards [`TopK::into_batch`]'s return values on failure.
    fn advance_to_top_k(&mut self, limit: std::num::NonZeroU32) -> Result<bool, ReadError> {
        if self.is_top_k_collected {
            self.gathered = None;
            return Ok(false);
        }
        self.is_top_k_collected = true;
        let mut top_k = TopK::new(limit);
        self.offer_every_record(&mut top_k)?;
        let gathered = top_k.into_batch()?;
        if 0 == gathered.num_records() {
            return Ok(false);
        }
        self.gathered = Some(gathered);
        self.has_current = true;
        Ok(true)
    }

    /// Offers every selected record of the remaining windows, tables and streams to `top_k`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::offer_window_rows`]'s return values on failure.
    /// * Forwards [`Self::open_window`]'s return values on failure.
    /// * Forwards [`Self::open_table`]'s return values on failure.
    /// * Forwards [`Self::open_stream`]'s return values on failure.
    fn offer_every_record(&mut self, top_k: &mut TopK) -> Result<(), ReadError> {
        loop {
            if self.is_window_active {
                self.is_window_active = false;
                self.offer_window_rows(top_k)?;
                continue;
            }
            if self.open_window()? || self.open_table()? || self.open_stream()? {
                continue;
            }
            return Ok(());
        }
    }

    /// Offers the selected rows of the current window to `top_k`, rendering a row only if
    /// `top_k` keeps it.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ReadError::Internal`] if a window is active without a table, or a rendered record is
    ///   missing.
    /// * Forwards [`clp_s_table::TableBatch::record_timestamp_ns`]'s return values on failure.
    /// * Forwards [`clp_s_table::TableBatch::record_id`]'s return values on failure.
    /// * Forwards [`clp_s_table::TableBatch::log_event_idx`]'s return values on failure.
    /// * Forwards [`JsonTemplate::bind`]'s return values on failure.
    /// * Forwards [`clp_s_json::BoundJsonTemplate::write_rows`]'s return values on failure.
    fn offer_window_rows(&mut self, top_k: &mut TopK) -> Result<(), ReadError> {
        let Some(table) = &self.table else {
            return Err(ReadError::Internal(
                "a window is active without a table".into(),
            ));
        };
        let batch = self.scanner.batch(&table.state);
        let archive_id = &self.archive.info().archive_id;
        let is_log_event_idx_needed = self.request.outputs().is_log_event_idx_needed;
        let BatchBuffers {
            selected_rows,
            json,
            ..
        } = &mut self.buffers;
        let mut bound = None;
        for row in selected_rows.iter().copied() {
            let timestamp_ns = batch.record_timestamp_ns(row)?;
            let key_timestamp = timestamp_ns.map(LatestFirstKey::timestamp_ms_of);
            let record_id = batch.record_id(row)?;
            if !top_k.admits(key_timestamp, archive_id, record_id) {
                continue;
            }
            let rendered = match &table.template {
                None => None,
                Some(template) => {
                    let bound = match &mut bound {
                        Some(bound) => bound,
                        None => bound.insert(template.bind(batch, self.archive.render_context())?),
                    };
                    json.clear();
                    bound.write_rows(std::slice::from_ref(&row), json)?;
                    let text = json.record(0).ok_or_else(|| {
                        ReadError::Internal("a rendered top-k record is missing".into())
                    })?;
                    self.stats.num_records_rendered += 1;
                    Some(Box::from(text))
                }
            };
            let log_event_idx = if is_log_event_idx_needed {
                batch.log_event_idx(row)?
            } else {
                None
            };
            top_k.offer(TopKRecord {
                key: LatestFirstKey {
                    timestamp_ms: key_timestamp,
                    archive_id: archive_id.clone(),
                    record_id,
                },
                timestamp_ns,
                log_event_idx,
                json: rendered,
            });
        }
        Ok(())
    }

    /// Materializes the next batch of the current window's selected rows: every remaining row,
    /// or, with JSON, rows until the rendered records reach the configured batch size.
    ///
    /// # Returns
    ///
    /// Whether a batch was materialized on success; `false` once the window is exhausted.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ReadError::Internal`] if a window is active without a table.
    /// * Forwards [`JsonTemplate::bind`]'s return values on failure.
    /// * Forwards [`clp_s_json::BoundJsonTemplate::write_rows`]'s return values on failure.
    /// * Forwards [`clp_s_table::TableBatch::record_id`]'s return values on failure.
    /// * Forwards [`clp_s_table::TableBatch::record_timestamp_ns`]'s return values on failure.
    /// * Forwards [`clp_s_table::TableBatch::log_event_idx`]'s return values on failure.
    fn deliver_window_rows(&mut self) -> Result<bool, ReadError> {
        let begin = self.next_selected;
        if !self.is_window_active || begin >= self.buffers.selected_rows.len() {
            self.is_window_active = false;
            return Ok(false);
        }
        let Some(table) = &self.table else {
            return Err(ReadError::Internal(
                "a window is active without a table".into(),
            ));
        };
        let batch = self.scanner.batch(&table.state);
        let BatchBuffers {
            selected_rows,
            delivered_rows,
            record_ids,
            timestamps_ns,
            log_event_indices,
            json,
        } = &mut self.buffers;
        let rows = &selected_rows[begin..];
        let num_rows = if let Some(template) = &table.template {
            let max_rendered_batch_size = self.archive.context().config().max_rendered_batch_size;
            json.clear();
            let bound = template.bind(batch, self.archive.render_context())?;
            let mut num_rows = 0;
            for row in rows {
                bound.write_rows(std::slice::from_ref(row), json)?;
                num_rows += 1;
                if json.byte_size() >= max_rendered_batch_size {
                    break;
                }
            }
            self.stats.num_records_rendered += num_rows as u64;
            num_rows
        } else {
            rows.len()
        };
        let rows = &rows[..num_rows];

        let outputs = self.request.outputs();
        record_ids.clear();
        timestamps_ns.clear();
        log_event_indices.clear();
        for row in rows {
            record_ids.push(batch.record_id(*row)?);
        }
        if outputs.is_timestamp_needed {
            for row in rows {
                timestamps_ns.push(batch.record_timestamp_ns(*row)?);
            }
        }
        if outputs.is_log_event_idx_needed {
            for row in rows {
                log_event_indices.push(batch.log_event_idx(*row)?);
            }
        }
        *delivered_rows = begin..begin + num_rows;
        self.next_selected = delivered_rows.end;
        Ok(true)
    }

    /// Decodes windows of the current table until one has a selected row.
    ///
    /// # Returns
    ///
    /// Whether a window with a selected row is active on success; `false` once the table is
    /// exhausted (or there is none).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ReadError::Resource`] if the scan's token is cancelled.
    /// * Forwards [`TableScanner::next_window`]'s return values on failure.
    /// * Forwards [`CompiledTableFilter::evaluate`]'s return values on failure.
    /// * Forwards [`TableScanner::finish_window`]'s return values on failure.
    fn open_window(&mut self) -> Result<bool, ReadError> {
        loop {
            let Some(table) = &mut self.table else {
                return Ok(false);
            };
            let candidate = &self.candidates[table.candidate];
            self.cancellation_token.check()?;
            let Some(window) = self.scanner.next_window(&mut table.state)? else {
                self.table = None;
                return Ok(false);
            };
            self.stats.num_rows_scanned += u64::from(window.columns.num_rows());
            match &candidate.filter {
                None => window.selected.select_all(),
                Some(filter) => filter.evaluate(window, &mut self.scratch)?,
            }
            if table.state.is_output_deferred()
                && 0 != self.scanner.batch(&table.state).selection().count()
            {
                self.load_table_tail()?;
            }
            let Some(table) = &self.table else {
                return Err(ReadError::Internal(
                    "the table being scanned is missing".into(),
                ));
            };
            let num_selected = self.scanner.finish_window(&table.state)?;
            if 0 == num_selected {
                continue;
            }
            self.stats.num_records_selected += u64::from(num_selected);
            self.buffers.selected_rows.clear();
            self.scanner
                .batch(&table.state)
                .selection()
                .extend_indices(&mut self.buffers.selected_rows);
            self.next_selected = 0;
            self.is_window_active = true;
            return Ok(true);
        }
    }

    /// Loads the next needed table of the current stream and starts scanning it.
    ///
    /// # Returns
    ///
    /// Whether a table was started on success; `false` once the stream is exhausted (or there is
    /// none).
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ReadError::Resource`] if the scan's token is cancelled.
    /// * [`ReadError::Internal`] if the table is missing from the directory.
    /// * Forwards [`TableBytes::new`]'s return values on failure.
    /// * Forwards [`TableBytes::new_prefix`]'s return values on failure.
    /// * Forwards [`Self::candidate_index`]'s return values on failure.
    /// * Forwards [`Table::load`]'s return values on failure.
    /// * Forwards [`clp_s_json::RenderContext::template`]'s return values on failure.
    fn open_table(&mut self) -> Result<bool, ReadError> {
        let Some(stream) = &mut self.stream else {
            return Ok(false);
        };
        let Some(ordinal) = stream.tables.next() else {
            self.stream = None;
            return Ok(false);
        };
        self.cancellation_token.check()?;
        let tables = self.archive.archive_tables();
        let info =
            *tables.index().tables().table(ordinal).ok_or_else(|| {
                ReadError::Internal(format!("table {ordinal:?} is missing").into())
            })?;
        let bytes = if stream.reader.is_some() {
            TableBytes::new_prefix(&stream.bytes, info)
        } else {
            TableBytes::new(&stream.bytes, info)
        }
        .map_err(TableError::from)?;
        let candidate = self.candidate_index(ordinal)?;
        let table = Arc::new(Table::load(Arc::clone(tables), bytes)?);
        let template = if self.request.outputs().is_json_needed {
            Some(self.archive.render_context().template(
                ordinal,
                &self.projection,
                &self.request.json_options(),
            )?)
        } else {
            None
        };
        let state = self
            .scanner
            .start(table, Arc::clone(&self.candidates[candidate].demand));
        self.table = Some(TableCursor {
            state,
            candidate,
            template,
        });
        self.stats.num_tables_scanned += 1;
        Ok(true)
    }

    /// Releases the current stream and decodes the next planned one: up to the end of its last
    /// table, or, if the scan reads only the leading columns of that table, up to the end of the
    /// columns needed before any of its rows is selected.
    ///
    /// # Returns
    ///
    /// Whether a stream was decoded on success; `false` once every planned stream was read.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ReadError::Resource`] if the scan's token is cancelled.
    /// * Forwards [`Self::leading_columns_to_decode`]'s return values on failure.
    /// * Forwards [`Self::acquire_stream_buffer`]'s return values on failure.
    /// * Forwards [`clp_s_io::ArchiveStorage::read_stream`]'s return values on failure.
    /// * Forwards [`clp_s_io::ArchiveStorage::begin_stream`]'s return values on failure.
    /// * Forwards [`LeadingColumns::begin_decode`]'s return values on failure.
    fn open_stream(&mut self) -> Result<bool, ReadError> {
        self.stream = None;
        let Some(read) = self.reads.next() else {
            return Ok(false);
        };
        self.cancellation_token.check()?;
        let leading_columns = self.leading_columns_to_decode(&read)?;
        let buffer = self.acquire_stream_buffer(read.decoded_prefix_size)?;
        let options = self.stream_read_options();
        let storage = self.archive.storage();
        let num_bytes_read_before = storage.num_bytes_read();
        let result = match &leading_columns {
            None => storage
                .read_stream(&read, &mut self.decoder, buffer, &options)
                .map(|bytes| (bytes, None))
                .map_err(ReadError::from),
            Some(leading_columns) => leading_columns
                .begin_decode(storage, &mut self.decoder, &read, buffer, &options)
                .map(|mut reader| (reader.share(), Some(reader))),
        };
        self.stats.num_bytes_read += storage
            .num_bytes_read()
            .saturating_sub(num_bytes_read_before);
        let (bytes, reader) = result?;
        self.stats.num_streams_decoded += 1;
        self.stats.num_decoded_bytes += reader
            .as_ref()
            .map_or(bytes.len() as u64, StreamReader::num_decoded_bytes);
        self.stream = Some(StreamCursor {
            bytes,
            tables: read.tables.into_iter(),
            decoded_prefix_size: read.decoded_prefix_size,
            reader,
        });
        Ok(true)
    }

    /// Decodes the rest of the current table, which was loaded from a prefix of its bytes without
    /// some output columns, and resumes its scan at the current window.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ReadError::Internal`] if no table, stream or stream decode is in progress.
    /// * Forwards [`clp_s_table::ArchiveTables::layout`]'s return values on failure.
    /// * Forwards [`Self::acquire_stream_buffer`]'s return values on failure.
    /// * Forwards [`StreamReader::restart`]'s return values on failure.
    /// * Forwards [`LeadingColumns::decode`]'s return values on failure.
    /// * Forwards [`TableBytes::new_prefix`]'s return values on failure.
    /// * Forwards [`Table::load_after`]'s return values on failure.
    /// * Forwards [`TableScanner::resume`]'s return values on failure.
    fn load_table_tail(&mut self) -> Result<(), ReadError> {
        let missing = || ReadError::Internal("a deferred table has no stream decode".into());
        let TableCursor {
            state,
            candidate,
            template,
        } = self.table.take().ok_or_else(missing)?;
        let info = *state.table().info();
        let loaded_columns = state.table().loaded_columns();
        let suspended = state.suspend();
        let (mut reader, decoded_prefix_size) = {
            let stream = self.stream.as_mut().ok_or_else(missing)?;
            stream.bytes = SharedBytes::new();
            (
                stream.reader.take().ok_or_else(missing)?,
                stream.decoded_prefix_size,
            )
        };
        let tables = Arc::clone(self.archive.archive_tables());
        let layout = tables.layout(info.ordinal)?;
        let num_decoded_bytes_before = reader.num_decoded_bytes();
        if !reader.try_reclaim() {
            let buffer = self.acquire_stream_buffer(decoded_prefix_size)?;
            reader.restart(&mut self.decoder, buffer)?;
        }
        let options = self.stream_read_options();
        let storage = self.archive.storage();
        let num_bytes_read_before = storage.num_bytes_read();
        let leading_columns = LeadingColumns {
            info,
            layout,
            num_columns: self.candidates[candidate].num_demanded_columns,
        };
        let result = leading_columns.decode(storage, &mut self.decoder, &mut reader, &options);
        self.stats.num_bytes_read += storage
            .num_bytes_read()
            .saturating_sub(num_bytes_read_before);
        result?;
        self.stats.num_decoded_bytes += reader
            .num_decoded_bytes()
            .saturating_sub(num_decoded_bytes_before);
        let bytes = reader.share();
        let table_bytes = TableBytes::new_prefix(&bytes, info).map_err(TableError::from)?;
        self.stream.as_mut().ok_or_else(missing)?.bytes = bytes;
        let table = Arc::new(Table::load_after(tables, table_bytes, loaded_columns)?);
        let state = self.scanner.resume(suspended, table)?;
        self.table = Some(TableCursor {
            state,
            candidate,
            template,
        });
        Ok(())
    }

    /// Decides how much of the last table of `read` to decode before scanning it.
    ///
    /// # Returns
    ///
    /// On success, the leading columns of the last table to decode first, or `None` if the scan
    /// reads all of its columns: its filter columns if rows may be rejected and other demanded
    /// columns follow them, else every demanded column.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ReadError::Internal`] if the table is missing from the directory.
    /// * Forwards [`Self::candidate_index`]'s return values on failure.
    /// * Forwards [`clp_s_table::ArchiveTables::layout`]'s return values on failure.
    fn leading_columns_to_decode(
        &self,
        read: &StreamRead,
    ) -> Result<Option<LeadingColumns>, ReadError> {
        let Some(&ordinal) = read.tables.last() else {
            return Ok(None);
        };
        let candidate = &self.candidates[self.candidate_index(ordinal)?];
        let num_columns = if candidate.filter.is_some()
            && candidate.num_evaluation_columns < candidate.num_demanded_columns
        {
            candidate.num_evaluation_columns
        } else {
            candidate.num_demanded_columns
        };
        if num_columns == candidate.num_columns {
            return Ok(None);
        }
        let tables = self.archive.archive_tables();
        let info =
            *tables.index().tables().table(ordinal).ok_or_else(|| {
                ReadError::Internal(format!("table {ordinal:?} is missing").into())
            })?;
        Ok(Some(LeadingColumns {
            info,
            layout: tables.layout(ordinal)?,
            num_columns,
        }))
    }

    /// # Returns
    ///
    /// The index in the scan's candidates of table `ordinal` on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ReadError::Internal`] if the read plan names a table that is not a candidate.
    fn candidate_index(&self, ordinal: TableOrdinal) -> Result<usize, ReadError> {
        self.candidates
            .binary_search_by_key(&ordinal, |candidate| candidate.ordinal)
            .map_err(|_| {
                ReadError::Internal(
                    format!("the read plan names table {ordinal:?}, which is not a candidate")
                        .into(),
                )
            })
    }

    /// Reserves `size` bytes of the memory budget and acquires a pooled buffer for them.
    ///
    /// # Returns
    ///
    /// The buffer on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`clp_s_resources::MemoryBudget::try_reserve`]'s return values on failure.
    /// * Forwards [`clp_s_resources::BufferPool::acquire`]'s return values on failure.
    fn acquire_stream_buffer(&self, size: u64) -> Result<PooledBuffer, ReadError> {
        let context = self.archive.context();
        let reservation = context.memory_budget().try_reserve(size)?;
        Ok(context.buffer_pool().acquire(size, reservation)?)
    }

    /// # Returns
    ///
    /// The options of the scan's stream reads.
    fn stream_read_options(&self) -> StreamReadOptions {
        StreamReadOptions {
            chunk_size: self.archive.context().config().stream_chunk_size,
            cancellation_token: self.cancellation_token.clone(),
        }
    }

    /// Drops every cursor so the scan delivers nothing more.
    fn finish(&mut self) {
        self.is_window_active = false;
        self.table = None;
        self.stream = None;
        self.reads = Vec::new().into_iter();
    }
}

impl std::fmt::Debug for Scan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Scan")
            .field("archive_id", &self.archive.info().archive_id)
            .field("termination_stage", &self.termination_stage)
            .field("prune_reason", &self.prune_reason)
            .field("num_candidates", &self.candidates.len())
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

/// One candidate table of a scan.
#[derive(Debug)]
struct Candidate {
    ordinal: TableOrdinal,

    /// The compiled residual filter, or `None` if every row matches.
    filter: Option<CompiledTableFilter>,

    demand: Arc<ColumnDemand>,
    dictionary_needs: DictionaryNeeds,

    /// The number of leading columns that hold every column the filter reads.
    num_evaluation_columns: usize,

    /// The number of leading columns that hold every column the scan reads.
    num_demanded_columns: usize,

    num_columns: usize,
}

impl Candidate {
    /// Factory function.
    ///
    /// Compiles the filter of table `ordinal` (`None` if every row matches) and derives its column
    /// demand: the filter's columns for evaluation; the projected columns (with JSON), the
    /// record-timestamp column (with timestamps or under [`ResultOrder::LatestFirst`]) for output.
    ///
    /// # Returns
    ///
    /// The candidate on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`clp_s_table::ArchiveTables::layout`]'s return values on failure.
    /// * Forwards [`CompiledTableFilter::compile`]'s return values on failure.
    fn compile(
        scan: &Scan,
        ordinal: TableOrdinal,
        filter: Option<(&ArchiveSearchPlan, &TableFilterPlan)>,
    ) -> Result<Self, ReadError> {
        let layout = scan.archive.archive_tables().layout(ordinal)?;
        let filter = match filter {
            None | Some((_, TableFilterPlan::MatchAll)) => None,
            Some((plan, TableFilterPlan::Residual(residual))) => {
                Some(CompiledTableFilter::compile(plan, residual, &layout)?)
            }
        };
        let num_columns = layout.num_columns();
        let evaluation = filter.as_ref().map_or_else(
            || ColumnSet::empty(num_columns),
            |filter| filter.required_columns().clone(),
        );
        let outputs = scan.request.outputs();
        let mut output = if outputs.is_json_needed {
            scan.projection.output_columns(&layout)
        } else {
            ColumnSet::empty(num_columns)
        };
        if scan.request.is_timestamp_needed()
            && let Some(column) = layout.record_timestamp_column()
        {
            output.insert(column);
        }
        let num_leading_columns = |columns: &ColumnSet| {
            columns
                .iter()
                .last()
                .map_or(0, |column| column.get() as usize + 1)
        };
        let num_evaluation_columns = num_leading_columns(&evaluation);
        let mut demanded = output.clone();
        demanded.union_with(&evaluation);
        if let Some(column) = layout.log_event_idx_column() {
            demanded.insert(column);
        }
        let demand = ColumnDemand::new(evaluation, output);
        let dictionary_needs = demand.dictionary_needs(&layout);
        Ok(Self {
            ordinal,
            filter,
            demand: Arc::new(demand),
            dictionary_needs,
            num_evaluation_columns,
            num_demanded_columns: num_leading_columns(&demanded),
            num_columns: layout.columns().len(),
        })
    }
}

/// The outcome of planning one archive.
enum Planned {
    Pruned(PruneReport),

    /// Every table, every row.
    FullScan,

    Search(Arc<ArchiveSearchPlan>),
}

/// The decoded stream whose tables are being scanned.
#[derive(Debug)]
struct StreamCursor {
    /// The decoded prefix of the stream.
    bytes: SharedBytes,

    /// The needed tables of the stream not started yet.
    tables: std::vec::IntoIter<TableOrdinal>,

    /// The planned prefix of the stream: the end of its last needed table.
    decoded_prefix_size: u64,

    /// The decode of the stream, kept while its last table is decoded only up to some leading
    /// columns.
    reader: Option<StreamReader>,
}

/// The leading columns of one table to decode.
#[derive(Debug)]
struct LeadingColumns {
    info: TableInfo,
    layout: Arc<TableLayout>,
    num_columns: usize,
}

impl LeadingColumns {
    /// Begins the decode of `read`, whose last table these columns belong to, and decodes it
    /// through them.
    ///
    /// # Returns
    ///
    /// The stream decode on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`ArchiveStorage::begin_stream`]'s return values on failure.
    /// * Forwards [`Self::decode`]'s return values on failure.
    fn begin_decode(
        &self,
        storage: &ArchiveStorage,
        decoder: &mut StreamDecoder,
        read: &StreamRead,
        buffer: PooledBuffer,
        options: &StreamReadOptions,
    ) -> Result<StreamReader, ReadError> {
        let mut reader = storage.begin_stream(read, decoder, buffer, options)?;
        self.decode(storage, decoder, &mut reader, options)?;
        Ok(reader)
    }

    /// Decodes `reader` until it holds these columns, measuring them from the bytes as they are
    /// decoded.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`TableLayout::leading_columns_size`]'s return values on failure.
    /// * Forwards [`ArchiveStorage::decode_stream_to`]'s return values on failure.
    fn decode(
        &self,
        storage: &ArchiveStorage,
        decoder: &mut StreamDecoder,
        reader: &mut StreamReader,
        options: &StreamReadOptions,
    ) -> Result<(), ReadError> {
        let table_begin = usize::try_from(self.info.stream_offset).unwrap_or(usize::MAX);
        loop {
            let prefix = reader.decoded().get(table_begin..).unwrap_or_default();
            let size = self
                .layout
                .leading_columns_size(&self.info, prefix, self.num_columns)?;
            let (size, is_known) = match size {
                LeadingColumnsSize::Known(size) => (size, true),
                LeadingColumnsSize::AtLeast(size) => (size, false),
            };
            storage.decode_stream_to(reader, decoder, self.info.stream_offset + size, options)?;
            if is_known {
                return Ok(());
            }
        }
    }
}

/// The table being scanned.
#[derive(Debug)]
struct TableCursor {
    state: TableScanState,

    /// The index of the table in the scan's candidates.
    candidate: usize,

    /// The template rendering the table's records, if JSON is needed.
    template: Option<Arc<JsonTemplate>>,
}

/// Reusable per-scan buffers of the delivered batch.
#[derive(Debug, Default)]
struct BatchBuffers {
    /// The selected rows of the current window.
    selected_rows: Vec<u32>,

    /// The positions in `selected_rows` of the rows of the delivered batch.
    delivered_rows: Range<usize>,

    record_ids: Vec<RecordId>,
    timestamps_ns: Vec<Option<i64>>,
    log_event_indices: Vec<Option<i64>>,
    json: JsonRecordBuffer,
}
