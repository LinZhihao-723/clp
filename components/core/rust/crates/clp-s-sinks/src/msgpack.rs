//! The msgpack search-result sink (file and network outputs).

use std::io::Write;
use std::sync::Arc;

use clp_s_reader::MergeOrder;
use clp_s_reader::OutputRequirements;
use clp_s_reader::PartialSink;
use clp_s_reader::PartitionInfo;
use clp_s_reader::PartitionSummary;
use clp_s_reader::RecordBatch;
use clp_s_reader::Sink;
use clp_s_reader::SinkControl;
use clp_s_reader::SinkError;
use clp_s_wire::SearchResultTuple;
use clp_s_wire::epoch_ns_to_wire_ms;
use parking_lot::Mutex;

use crate::json_lines::write_error;

/// Writes every record as a [`clp_s_wire::SearchResultTuple`] to one writer opened once per
/// session (DV-21, DV-22), buffered per batch.
///
/// # Type Parameters
///
/// * `WriterType` - The destination (a file, or a `TcpStream` to the network peer).
#[derive(Debug)]
pub struct MsgpackResultSink<WriterType: Write + Send + 'static> {
    writer: Arc<Mutex<WriterType>>,
}

impl<WriterType: Write + Send + 'static> MsgpackResultSink<WriterType> {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A sink writing to `writer`.
    #[must_use]
    pub fn new(writer: WriterType) -> Self {
        Self {
            writer: Arc::new(Mutex::new(writer)),
        }
    }

    /// Takes the writer back once every partial sink of the sink is gone (after a session run).
    ///
    /// # Returns
    ///
    /// The writer on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Self`] (the sink itself) if a partial sink still shares the writer.
    pub fn into_writer(self) -> Result<WriterType, Self> {
        Arc::try_unwrap(self.writer)
            .map(Mutex::into_inner)
            .map_err(|writer| Self { writer })
    }
}

impl<WriterType: Write + Send + 'static> Sink for MsgpackResultSink<WriterType> {
    type Output = u64;
    type Partial = MsgpackResultPartial<WriterType>;

    fn requirements(&self) -> OutputRequirements {
        OutputRequirements::json()
            .with_timestamps()
            .with_log_event_indices()
    }

    fn merge_order(&self) -> MergeOrder {
        MergeOrder::Commutative
    }

    fn open_partial(&self, _partition: &PartitionInfo) -> Result<Self::Partial, SinkError> {
        Ok(MsgpackResultPartial {
            writer: Arc::clone(&self.writer),
            buffer: Vec::new(),
            num_records: 0,
        })
    }

    fn merge(&self, output: &mut Self::Output, partial: u64) -> Result<SinkControl, SinkError> {
        *output += partial;
        Ok(SinkControl::Continue)
    }
}

/// The partial sink of [`MsgpackResultSink`].
///
/// # Type Parameters
///
/// * `WriterType` - The destination shared with the sink.
#[derive(Debug)]
pub struct MsgpackResultPartial<WriterType: Write + Send + 'static> {
    writer: Arc<Mutex<WriterType>>,

    /// The encoded tuples of the batch being consumed.
    buffer: Vec<u8>,

    num_records: u64,
}

impl<WriterType: Write + Send + 'static> PartialSink for MsgpackResultPartial<WriterType> {
    type Output = u64;

    fn consume(&mut self, batch: &RecordBatch<'_>) -> Result<SinkControl, SinkError> {
        let missing = |what: &str| SinkError::Encode(format!("the batch has no {what}").into());
        let json = batch.json().ok_or_else(|| missing("rendered records"))?;
        let timestamps_ns = batch.timestamps_ns().ok_or_else(|| missing("timestamps"))?;
        let log_event_indices = batch
            .log_event_indices()
            .ok_or_else(|| missing("log event indices"))?;
        self.buffer.clear();
        for record in 0..batch.num_records() {
            SearchResultTuple {
                timestamp_ms: timestamps_ns[record].map_or(0, epoch_ns_to_wire_ms),
                message: json
                    .record(record)
                    .ok_or_else(|| missing("record text for every record"))?,
                orig_file_path: "",
                archive_id: batch.archive_id(record).as_str(),
                log_event_idx: log_event_indices[record].unwrap_or(0),
            }
            .encode(&mut self.buffer)
            .map_err(|error| SinkError::Encode(error.to_string().into()))?;
        }
        self.writer
            .lock()
            .write_all(&self.buffer)
            .map_err(write_error)?;
        self.num_records += batch.num_records() as u64;
        Ok(SinkControl::Continue)
    }

    fn finish(self, _summary: &PartitionSummary) -> Result<Self::Output, SinkError> {
        self.writer.lock().flush().map_err(write_error)?;
        Ok(self.num_records)
    }
}
