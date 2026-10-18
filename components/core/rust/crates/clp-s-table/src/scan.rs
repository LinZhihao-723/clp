//! Window-at-a-time scanning of loaded tables with owned scan state.

use std::cell::Cell;
use std::num::NonZeroU32;
use std::sync::Arc;

use crate::ColumnBatch;
use crate::ColumnDemand;
use crate::ColumnIndex;
use crate::ColumnSet;
use crate::LeI64;
use crate::RecordId;
use crate::RecordView;
use crate::Selection;
use crate::Table;
use crate::TableError;

/// Owned position of one scan inside one table; holds no borrows, so a cursor can keep it across
/// calls.
#[derive(Debug)]
pub struct TableScanState {
    table: Arc<Table>,
    demand: Arc<ColumnDemand>,

    /// The columns decoded by [`TableScanner::finish_window`]: the demanded output columns plus
    /// the `log_event_idx` column (which record ids read), minus the evaluation columns.
    output_only: ColumnSet,

    anchors: Box<[DeltaAnchor]>,

    /// Identifies this scan to its scanner, which holds the decoded window of one scan at a time.
    scan_id: u64,

    next_row: u32,
}

impl TableScanState {
    /// # Returns
    ///
    /// The table being scanned.
    #[must_use]
    pub const fn table(&self) -> &Arc<Table> {
        &self.table
    }

    /// # Returns
    ///
    /// Whether every window has been returned.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.next_row >= self.table.num_rows()
    }
}

/// Reusable per-worker scan buffers: decoded delta windows and the selection.
///
/// The scanner holds the decoded window of the scan that last called [`Self::next_window`];
/// finishing or reading the window of another scan is a caller bug and panics.
#[derive(Debug)]
pub struct TableScanner {
    batch_num_rows: NonZeroU32,

    /// The prefix-summed window of each decoded delta-encoded column, by column index.
    decoded: Vec<Vec<i64>>,

    selection: Selection,
    window: Option<Window>,
    next_scan_id: Cell<u64>,
}

impl TableScanner {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A scanner whose windows hold at most `batch_num_rows` rows.
    #[must_use]
    pub fn new(batch_num_rows: NonZeroU32) -> Self {
        Self {
            batch_num_rows,
            decoded: Vec::new(),
            selection: Selection::empty(0),
            window: None,
            next_scan_id: Cell::new(0),
        }
    }

    /// Starts a scan over every row of `table` that decodes only the columns of `demand`, plus
    /// the table's `log_event_idx` column for windows with a selected row (record ids read it).
    ///
    /// # Returns
    ///
    /// The state of the scan.
    ///
    /// # Panics
    ///
    /// Panics if `demand` was built for a layout with a different number of columns.
    #[must_use]
    pub fn start(&self, table: Arc<Table>, demand: Arc<ColumnDemand>) -> TableScanState {
        let layout = table.layout();
        assert_eq!(
            demand.evaluation().num_columns(),
            layout.num_columns(),
            "the demand was built for another layout"
        );
        let mut output_only = demand.output().clone();
        if let Some(column) = layout.log_event_idx_column() {
            output_only.insert(column);
        }
        output_only.subtract(demand.evaluation());
        let anchors = demand
            .evaluation()
            .iter()
            .map(|column| (column, true))
            .chain(output_only.iter().map(|column| (column, false)))
            .filter(|(column, _)| {
                layout
                    .column(*column)
                    .is_some_and(|spec| spec.encoding.is_delta_encoded())
            })
            .map(|(column, is_evaluation)| DeltaAnchor {
                column,
                is_evaluation,
                sum: 0,
                window_base: 0,
            })
            .collect();
        let scan_id = self.next_scan_id.get();
        self.next_scan_id.set(scan_id.wrapping_add(1));
        TableScanState {
            table,
            demand,
            output_only,
            anchors,
            scan_id,
            next_row: 0,
        }
    }

    /// Decodes the evaluation columns of the next window of `state`; at most `batch_num_rows` rows
    /// of work. The returned selection has no row selected.
    ///
    /// # Returns
    ///
    /// The window, or `None` once the table is exhausted, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Table::delta_window`]'s return values on failure.
    pub fn next_window<'window>(
        &'window mut self,
        state: &'window mut TableScanState,
    ) -> Result<Option<WindowParts<'window>>, TableError> {
        let table_num_rows = state.table.num_rows();
        if state.next_row >= table_num_rows {
            self.window = None;
            return Ok(None);
        }
        let first_row = state.next_row;
        let num_rows = self.batch_num_rows.get().min(table_num_rows - first_row);
        let rows = first_row..first_row + num_rows;
        state.next_row = rows.end;

        let num_columns = state.table.layout().columns().len();
        if self.decoded.len() < num_columns {
            self.decoded.resize_with(num_columns, Vec::new);
        }
        for anchor in &mut state.anchors {
            let deltas = state.table.delta_window(anchor.column, rows.clone())?;
            anchor.window_base = anchor.sum;
            anchor.sum = if anchor.is_evaluation {
                prefix_sum(
                    deltas,
                    anchor.window_base,
                    &mut self.decoded[anchor.column.get() as usize],
                )
            } else {
                deltas.iter().fold(anchor.window_base, |sum, delta| {
                    sum.wrapping_add(delta.get())
                })
            };
        }
        self.selection.reset(num_rows);
        self.window = Some(Window {
            scan_id: state.scan_id,
            first_row,
            num_rows,
            is_output_decoded: false,
        });

        let state: &'window TableScanState = state;
        Ok(Some(WindowParts {
            columns: WindowColumns {
                table: &state.table,
                decoded: &self.decoded,
                evaluation: state.demand.evaluation(),
                first_row,
                num_rows,
            },
            selected: &mut self.selection,
        }))
    }

    /// Decodes the output-only columns of the current window if at least one row is selected.
    ///
    /// # Returns
    ///
    /// The number of selected rows on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Table::delta_window`]'s return values on failure.
    ///
    /// # Panics
    ///
    /// Panics if the scanner's current window is not a window of `state`.
    pub fn finish_window(&mut self, state: &TableScanState) -> Result<u32, TableError> {
        let window = self.window_of(state);
        let num_selected = self.selection.count();
        if 0 == num_selected || window.is_output_decoded {
            return Ok(num_selected);
        }
        let rows = window.first_row..window.first_row + window.num_rows;
        for anchor in state.anchors.iter().filter(|anchor| !anchor.is_evaluation) {
            let deltas = state.table.delta_window(anchor.column, rows.clone())?;
            prefix_sum(
                deltas,
                anchor.window_base,
                &mut self.decoded[anchor.column.get() as usize],
            );
        }
        if let Some(window) = &mut self.window {
            window.is_output_decoded = true;
        }
        Ok(num_selected)
    }

    /// # Returns
    ///
    /// The finished current window as a borrowed batch.
    ///
    /// # Panics
    ///
    /// Panics if the scanner's current window is not a window of `state`.
    #[must_use]
    pub fn batch<'batch>(&'batch self, state: &'batch TableScanState) -> TableBatch<'batch> {
        let window = self.window_of(state);
        TableBatch {
            table: &state.table,
            decoded: &self.decoded,
            selection: &self.selection,
            evaluation: state.demand.evaluation(),
            output: window.is_output_decoded.then_some(&state.output_only),
            first_row: window.first_row,
            num_rows: window.num_rows,
        }
    }

    /// # Returns
    ///
    /// The current window, which must belong to `state`.
    ///
    /// # Panics
    ///
    /// Panics if the scanner has no current window of `state`.
    fn window_of(&self, state: &TableScanState) -> Window {
        match self.window {
            Some(window) if window.scan_id == state.scan_id => window,
            _ => panic!("the scanner's current window does not belong to this scan"),
        }
    }
}

/// The evaluator's view of a window: the evaluation columns to read and the selection to write.
#[derive(Debug)]
pub struct WindowParts<'window> {
    pub columns: WindowColumns<'window>,
    pub selected: &'window mut Selection,
}

/// The evaluation columns of one window.
#[derive(Clone, Copy, Debug)]
pub struct WindowColumns<'window> {
    table: &'window Arc<Table>,
    decoded: &'window [Vec<i64>],
    evaluation: &'window ColumnSet,
    first_row: u32,
    num_rows: u32,
}

impl<'window> WindowColumns<'window> {
    /// # Returns
    ///
    /// The table being scanned.
    #[must_use]
    pub const fn table(&self) -> &'window Arc<Table> {
        self.table
    }

    /// # Returns
    ///
    /// The table row of window row 0.
    #[must_use]
    pub const fn first_row(&self) -> u32 {
        self.first_row
    }

    /// # Returns
    ///
    /// The number of rows in the window.
    #[must_use]
    pub const fn num_rows(&self) -> u32 {
        self.num_rows
    }

    /// Returns one evaluation column restricted to the window.
    ///
    /// # Returns
    ///
    /// The column on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::ColumnNotDecoded`] if `column` is not an evaluation column of the scan.
    /// * Forwards [`Table::column_batch`]'s return values on failure.
    pub fn column(&self, column: ColumnIndex) -> Result<ColumnBatch<'window>, TableError> {
        if !self.evaluation.contains(column) {
            return Err(TableError::ColumnNotDecoded(column));
        }
        self.table.column_batch(
            self.decoded,
            column,
            self.first_row..self.first_row + self.num_rows,
        )
    }
}

/// The finished window of one table: its selection plus every demanded column (borrowed).
#[derive(Clone, Copy, Debug)]
pub struct TableBatch<'batch> {
    table: &'batch Arc<Table>,
    decoded: &'batch [Vec<i64>],
    selection: &'batch Selection,
    evaluation: &'batch ColumnSet,

    /// The output-only columns, if they are decoded for this window.
    output: Option<&'batch ColumnSet>,

    first_row: u32,
    num_rows: u32,
}

impl<'batch> TableBatch<'batch> {
    /// # Returns
    ///
    /// The table being scanned.
    #[must_use]
    pub const fn table(&self) -> &'batch Arc<Table> {
        self.table
    }

    /// # Returns
    ///
    /// The table row of window row 0.
    #[must_use]
    pub const fn first_row(&self) -> u32 {
        self.first_row
    }

    /// # Returns
    ///
    /// The number of rows in the window.
    #[must_use]
    pub const fn num_rows(&self) -> u32 {
        self.num_rows
    }

    /// # Returns
    ///
    /// The selected rows of the window.
    #[must_use]
    pub const fn selection(&self) -> &'batch Selection {
        self.selection
    }

    /// # Returns
    ///
    /// Whether `column` is decoded in this window.
    #[must_use]
    pub fn is_decoded(&self, column: ColumnIndex) -> bool {
        self.evaluation.contains(column) || self.output.is_some_and(|set| set.contains(column))
    }

    /// Returns one demanded column restricted to the window.
    ///
    /// # Returns
    ///
    /// The column on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::ColumnNotDecoded`] if `column` is not demanded by the scan, or is an output
    ///   column of a window without a selected row.
    /// * Forwards [`Table::column_batch`]'s return values on failure.
    pub fn column(&self, column: ColumnIndex) -> Result<ColumnBatch<'batch>, TableError> {
        if !self.is_decoded(column) {
            return Err(TableError::ColumnNotDecoded(column));
        }
        self.table.column_batch(
            self.decoded,
            column,
            self.first_row..self.first_row + self.num_rows,
        )
    }

    /// Returns the record id of window row `row`: its `log_event_idx` if the table records log
    /// order, else its ordinal in table-directory order.
    ///
    /// # Returns
    ///
    /// The record id on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::log_event_idx`]'s return values on failure (the `log_event_idx` column is
    ///   decoded by [`TableScanner::finish_window`] for every window with a selected row).
    ///
    /// # Panics
    ///
    /// Panics if `row` is not below [`Self::num_rows`].
    pub fn record_id(&self, row: u32) -> Result<RecordId, TableError> {
        if let Some(log_event_idx) = self.log_event_idx(row)? {
            return Ok(RecordId::new(log_event_idx));
        }
        let ordinal =
            self.table.info().first_record_ordinal + u64::from(self.first_row) + u64::from(row);
        Ok(RecordId::new(i64::try_from(ordinal).unwrap_or(i64::MAX)))
    }

    /// Returns the `log_event_idx` of window row `row`; the column must be demanded.
    ///
    /// # Returns
    ///
    /// The index, or `None` if the archive does not record log order, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::integer_value`]'s return values on failure.
    ///
    /// # Panics
    ///
    /// Panics if `row` is not below [`Self::num_rows`].
    pub fn log_event_idx(&self, row: u32) -> Result<Option<i64>, TableError> {
        assert!(row < self.num_rows, "row {row} is outside the window");
        self.table
            .layout()
            .log_event_idx_column()
            .map(|column| self.integer_value(column, row))
            .transpose()
    }

    /// Returns the authoritative timestamp of window row `row`; the column must be demanded.
    ///
    /// # Returns
    ///
    /// The timestamp in epoch nanoseconds, or `None` if the table has no authoritative timestamp
    /// column, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::integer_value`]'s return values on failure.
    ///
    /// # Panics
    ///
    /// Panics if `row` is not below [`Self::num_rows`].
    pub fn record_timestamp_ns(&self, row: u32) -> Result<Option<i64>, TableError> {
        assert!(row < self.num_rows, "row {row} is outside the window");
        self.table
            .layout()
            .record_timestamp_column()
            .map(|column| self.integer_value(column, row))
            .transpose()
    }

    /// # Returns
    ///
    /// The record of window row `row`.
    ///
    /// # Panics
    ///
    /// Panics if `row` is not below [`Self::num_rows`].
    #[must_use]
    pub fn record(&self, row: u32) -> RecordView<'batch> {
        assert!(row < self.num_rows, "row {row} is outside the window");
        RecordView::new(*self, row)
    }

    /// Returns the `i64` of window row `row` of the integer, delta-encoded integer or timestamp
    /// column `column`.
    ///
    /// # Returns
    ///
    /// The value (epoch nanoseconds for a timestamp column) on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::NotIntegerColumn`] if `column` holds no `i64` values (the layout only names
    ///   integer `log_event_idx` and timestamp record-timestamp columns, so this indicates an
    ///   internal inconsistency).
    /// * Forwards [`Self::column`]'s return values on failure.
    ///
    /// # Panics
    ///
    /// Panics if `row` is not below [`Self::num_rows`].
    fn integer_value(&self, column: ColumnIndex, row: u32) -> Result<i64, TableError> {
        let index = row as usize;
        match self.column(column)? {
            ColumnBatch::Int64(values) => Ok(values[index].get()),
            ColumnBatch::DeltaInt64(values)
            | ColumnBatch::Timestamp {
                epoch_ns: values, ..
            } => Ok(values[index]),
            _ => Err(TableError::NotIntegerColumn(column)),
        }
    }
}

/// The decoded window of the scan that last called [`TableScanner::next_window`].
#[derive(Clone, Copy, Debug)]
struct Window {
    scan_id: u64,
    first_row: u32,
    num_rows: u32,
    is_output_decoded: bool,
}

/// The running prefix sum of one delta-encoded column of a scan.
#[derive(Debug)]
struct DeltaAnchor {
    column: ColumnIndex,
    is_evaluation: bool,

    /// The sum of the deltas of every row before [`TableScanState::next_row`].
    sum: i64,

    /// The sum of the deltas of every row before the current window.
    window_base: i64,
}

/// Writes the running sums of `deltas`, starting from `base`, into `output` (replacing its
/// contents): element `i` is `base + deltas[0] + ... + deltas[i]`, with wrapping arithmetic like
/// the C++ reader's.
///
/// # Returns
///
/// The sum after the last delta.
fn prefix_sum(deltas: &[LeI64], base: i64, output: &mut Vec<i64>) -> i64 {
    output.clear();
    output.reserve(deltas.len());
    let mut sum = base;
    for delta in deltas {
        sum = sum.wrapping_add(delta.get());
        output.push(sum);
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_sums_continue_from_the_base() {
        let deltas: Vec<LeI64> = [5_i64, -2, 10].into_iter().map(LeI64::new).collect();
        let mut output = vec![99; 8];
        assert_eq!(prefix_sum(&deltas, 100, &mut output), 113);
        assert_eq!(output, vec![105, 103, 113]);
        assert_eq!(prefix_sum(&[], 7, &mut output), 7);
        assert_eq!(output, Vec::<i64>::new());
    }

    #[test]
    fn prefix_sums_wrap_like_cpp() {
        let deltas = [LeI64::new(i64::MAX), LeI64::new(2)];
        let mut output = Vec::new();
        assert_eq!(prefix_sum(&deltas, 0, &mut output), i64::MIN + 1);
        assert_eq!(output, vec![i64::MAX, i64::MIN + 1]);
    }
}
