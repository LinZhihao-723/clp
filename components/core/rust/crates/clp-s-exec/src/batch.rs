//! Delivered batches.

use clp_s_json::JsonRecordBuffer;
use clp_s_table::RecordId;
use clp_s_table::TableBatch;

use crate::ArchiveId;
use crate::OutputRequirements;
use crate::ReadError;

/// The selected records of one delivery, indexed by selected-record ordinal `0..num_records()`.
///
/// Every per-record accessor uses that one index space; [`Self::json`] holds one record per
/// ordinal when JSON was requested.
#[derive(Debug)]
pub struct RecordBatch<'batch> {
    rows: BatchRows<'batch>,
    archive_ids: ArchiveIds<'batch>,
    record_ids: &'batch [RecordId],
    timestamps_ns: Option<&'batch [Option<i64>]>,
    log_event_indices: Option<&'batch [Option<i64>]>,
    json: Option<&'batch JsonRecordBuffer>,
}

impl<'batch> RecordBatch<'batch> {
    /// # Returns
    ///
    /// The storage behind the batch's records, in the batch's selected-record ordinal order.
    #[must_use]
    pub const fn rows(&self) -> &BatchRows<'batch> {
        &self.rows
    }

    /// # Returns
    ///
    /// The number of selected records.
    #[must_use]
    pub const fn num_records(&self) -> usize {
        self.record_ids.len()
    }

    /// # Returns
    ///
    /// The archive of record `record`.
    ///
    /// # Panics
    ///
    /// Panics if `record` is not below [`Self::num_records`].
    #[must_use]
    pub fn archive_id(&self, record: usize) -> &ArchiveId {
        assert!(
            record < self.num_records(),
            "record {record} is outside a batch of {} records",
            self.num_records()
        );
        match self.archive_ids {
            ArchiveIds::One(archive_id) => archive_id,
            ArchiveIds::PerRecord(archive_ids) => &archive_ids[record],
        }
    }

    /// # Returns
    ///
    /// The record id of every selected record.
    #[must_use]
    pub const fn record_ids(&self) -> &[RecordId] {
        self.record_ids
    }

    /// # Returns
    ///
    /// The authoritative timestamp (epoch ns, `None` if the record has none) of every selected
    /// record, or `None` if the request did not ask for timestamps.
    #[must_use]
    pub const fn timestamps_ns(&self) -> Option<&[Option<i64>]> {
        self.timestamps_ns
    }

    /// # Returns
    ///
    /// The `log_event_idx` (`None` if the archive does not record log order) of every selected
    /// record, or `None` if the request did not ask for them.
    #[must_use]
    pub const fn log_event_indices(&self) -> Option<&[Option<i64>]> {
        self.log_event_indices
    }

    /// # Returns
    ///
    /// The rendered records, or `None` if the request did not ask for JSON.
    #[must_use]
    pub const fn json(&self) -> Option<&JsonRecordBuffer> {
        self.json
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// A batch over the window rows `rows` of the table window `batch`, whose per-record columns
    /// are `columns`.
    pub(crate) const fn from_table(
        batch: TableBatch<'batch>,
        rows: &'batch [u32],
        archive_id: &'batch ArchiveId,
        columns: RecordColumns<'batch>,
    ) -> Self {
        Self {
            rows: BatchRows::Table { batch, rows },
            archive_ids: ArchiveIds::One(archive_id),
            record_ids: columns.record_ids,
            timestamps_ns: columns.timestamps_ns,
            log_event_indices: columns.log_event_indices,
            json: columns.json,
        }
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// A batch over the gathered records `batch`, exposing what `outputs` asks for.
    pub(crate) fn from_gathered(batch: &'batch OwnedBatch, outputs: OutputRequirements) -> Self {
        Self {
            rows: BatchRows::Gathered(batch),
            archive_ids: ArchiveIds::PerRecord(&batch.archive_ids),
            record_ids: &batch.record_ids,
            timestamps_ns: outputs
                .is_timestamp_needed
                .then_some(batch.timestamps_ns.as_slice()),
            log_event_indices: outputs
                .is_log_event_idx_needed
                .then_some(batch.log_event_indices.as_slice()),
            json: batch.json.as_ref().filter(|_| outputs.is_json_needed),
        }
    }
}

/// The storage behind a batch.
#[non_exhaustive]
#[derive(Debug)]
pub enum BatchRows<'batch> {
    /// Selected rows of one window of one table: `rows[i]` is the window row of selected record
    /// `i`.
    Table {
        batch: TableBatch<'batch>,
        rows: &'batch [u32],
    },

    /// Records gathered across tables and archives (the final top-k batch).
    Gathered(&'batch OwnedBatch),
}

/// Owned, compacted records; pins no stream buffer and crosses threads.
#[derive(Debug, Default)]
pub struct OwnedBatch {
    archive_ids: Vec<ArchiveId>,
    record_ids: Vec<RecordId>,
    timestamps_ns: Vec<Option<i64>>,
    log_event_indices: Vec<Option<i64>>,

    /// The rendered records, or `None` if no record carries JSON.
    json: Option<JsonRecordBuffer>,
}

impl OwnedBatch {
    /// # Returns
    ///
    /// The number of records.
    #[must_use]
    pub const fn num_records(&self) -> usize {
        self.record_ids.len()
    }

    /// Appends one record.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ReadError::InvalidArgument`] if `json` is `Some` while earlier records carry no JSON, or
    ///   `None` while they do.
    /// * Forwards [`JsonRecordBuffer::push_rendered`]'s return values on failure.
    pub fn push(
        &mut self,
        archive_id: ArchiveId,
        record_id: RecordId,
        timestamp_ns: Option<i64>,
        log_event_idx: Option<i64>,
        json: Option<&[u8]>,
    ) -> Result<(), ReadError> {
        let is_first = self.record_ids.is_empty();
        match (json, &mut self.json) {
            (Some(text), Some(buffer)) => buffer.push_rendered(text)?,
            (Some(text), None) if is_first => {
                let mut buffer = JsonRecordBuffer::new();
                buffer.push_rendered(text)?;
                self.json = Some(buffer);
            }
            (None, None) => {}
            _ => {
                return Err(ReadError::InvalidArgument(
                    "every record of an owned batch must carry JSON, or none".into(),
                ));
            }
        }
        self.archive_ids.push(archive_id);
        self.record_ids.push(record_id);
        self.timestamps_ns.push(timestamp_ns);
        self.log_event_indices.push(log_event_idx);
        Ok(())
    }
}

/// The per-record columns of one delivered batch, all of the same length.
#[derive(Clone, Copy, Debug)]
pub struct RecordColumns<'batch> {
    pub record_ids: &'batch [RecordId],
    pub timestamps_ns: Option<&'batch [Option<i64>]>,
    pub log_event_indices: Option<&'batch [Option<i64>]>,
    pub json: Option<&'batch JsonRecordBuffer>,
}

/// The archive ids of a batch's records.
#[derive(Clone, Copy, Debug)]
enum ArchiveIds<'batch> {
    /// Every record belongs to this archive.
    One(&'batch ArchiveId),

    /// `ids[i]` is the archive of record `i`.
    PerRecord(&'batch [ArchiveId]),
}

#[cfg(test)]
mod tests {
    use clp_s_table::RecordId;

    use super::OwnedBatch;
    use crate::ArchiveId;
    use crate::ReadError;

    #[test]
    fn mixing_records_with_and_without_json_is_rejected() -> anyhow::Result<()> {
        let archive_id = ArchiveId::from("a");
        for (first, second) in [(Some(&b"{}"[..]), None), (None, Some(&b"{}"[..]))] {
            let mut batch = OwnedBatch::default();
            batch.push(archive_id.clone(), RecordId::new(0), None, None, first)?;
            let error = batch
                .push(archive_id.clone(), RecordId::new(1), None, None, second)
                .expect_err("the second record disagrees on JSON");
            assert!(
                matches!(error, ReadError::InvalidArgument(_)),
                "{first:?}, {second:?}: {error:?}"
            );
            assert_eq!(batch.num_records(), 1);
        }
        Ok(())
    }
}
