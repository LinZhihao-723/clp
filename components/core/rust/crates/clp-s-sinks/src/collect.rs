//! The in-memory collecting sink.

use clp_s_reader::ArchiveId;
use clp_s_reader::MergeOrder;
use clp_s_reader::OutputRequirements;
use clp_s_reader::PartialSink;
use clp_s_reader::PartitionInfo;
use clp_s_reader::PartitionSummary;
use clp_s_reader::RecordBatch;
use clp_s_reader::RecordId;
use clp_s_reader::Sink;
use clp_s_reader::SinkControl;
use clp_s_reader::SinkError;

/// One collected record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectedRecord {
    pub archive_id: ArchiveId,
    pub record_id: RecordId,
    pub timestamp_ns: Option<i64>,
    pub log_event_idx: Option<i64>,

    /// The JSON text, without a trailing newline.
    pub json: Vec<u8>,
}

/// Collects every record in session scan order (tests, the differential harness).
#[derive(Debug, Default)]
pub struct CollectSink;

impl CollectSink {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A new collector.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Sink for CollectSink {
    type Output = Vec<CollectedRecord>;
    type Partial = CollectPartial;

    fn requirements(&self) -> OutputRequirements {
        OutputRequirements::json()
            .with_timestamps()
            .with_log_event_indices()
    }

    fn merge_order(&self) -> MergeOrder {
        MergeOrder::Scan
    }

    fn open_partial(&self, _partition: &PartitionInfo) -> Result<Self::Partial, SinkError> {
        Ok(CollectPartial {
            records: Vec::new(),
        })
    }

    fn merge(
        &self,
        output: &mut Self::Output,
        partial: Vec<CollectedRecord>,
    ) -> Result<SinkControl, SinkError> {
        output.extend(partial);
        Ok(SinkControl::Continue)
    }
}

/// The partial sink of [`CollectSink`].
#[derive(Debug)]
pub struct CollectPartial {
    records: Vec<CollectedRecord>,
}

impl PartialSink for CollectPartial {
    type Output = Vec<CollectedRecord>;

    fn consume(&mut self, batch: &RecordBatch<'_>) -> Result<SinkControl, SinkError> {
        let missing = |what: &str| SinkError::Encode(format!("the batch has no {what}").into());
        let json = batch.json().ok_or_else(|| missing("rendered records"))?;
        let timestamps_ns = batch.timestamps_ns().ok_or_else(|| missing("timestamps"))?;
        let log_event_indices = batch
            .log_event_indices()
            .ok_or_else(|| missing("log event indices"))?;
        for (record, record_id) in batch.record_ids().iter().enumerate() {
            let text = json
                .record(record)
                .ok_or_else(|| missing("record text for every record"))?;
            self.records.push(CollectedRecord {
                archive_id: batch.archive_id(record).clone(),
                record_id: *record_id,
                timestamp_ns: timestamps_ns[record],
                log_event_idx: log_event_indices[record],
                json: text.to_vec(),
            });
        }
        Ok(SinkControl::Continue)
    }

    fn finish(self, _summary: &PartitionSummary) -> Result<Self::Output, SinkError> {
        Ok(self.records)
    }
}
