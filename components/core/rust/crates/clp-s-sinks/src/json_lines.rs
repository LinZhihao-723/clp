//! The JSON-lines sink.

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
use parking_lot::Mutex;

/// Writes every record as one JSON line to a shared writer, in delivery order; a closed
/// downstream becomes [`SinkError::DownstreamClosed`].
///
/// # Type Parameters
///
/// * `WriterType` - The destination (stdout, a file opened once per session).
#[derive(Debug)]
pub struct JsonLinesSink<WriterType: Write + Send + 'static> {
    writer: Arc<Mutex<WriterType>>,
}

impl<WriterType: Write + Send + 'static> JsonLinesSink<WriterType> {
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

impl<WriterType: Write + Send + 'static> Sink for JsonLinesSink<WriterType> {
    type Output = u64;
    type Partial = JsonLinesPartial<WriterType>;

    fn requirements(&self) -> OutputRequirements {
        OutputRequirements::json()
    }

    fn merge_order(&self) -> MergeOrder {
        MergeOrder::Scan
    }

    fn open_partial(&self, _partition: &PartitionInfo) -> Result<Self::Partial, SinkError> {
        Ok(JsonLinesPartial {
            writer: Arc::clone(&self.writer),
            num_records: 0,
        })
    }

    fn merge(&self, output: &mut Self::Output, partial: u64) -> Result<SinkControl, SinkError> {
        *output += partial;
        Ok(SinkControl::Continue)
    }
}

/// The partial sink of [`JsonLinesSink`].
///
/// # Type Parameters
///
/// * `WriterType` - The destination shared with the sink.
#[derive(Debug)]
pub struct JsonLinesPartial<WriterType: Write + Send + 'static> {
    writer: Arc<Mutex<WriterType>>,
    num_records: u64,
}

impl<WriterType: Write + Send + 'static> PartialSink for JsonLinesPartial<WriterType> {
    type Output = u64;

    fn consume(&mut self, batch: &RecordBatch<'_>) -> Result<SinkControl, SinkError> {
        let json = batch
            .json()
            .ok_or_else(|| SinkError::Encode("the batch has no rendered records".into()))?;
        self.writer
            .lock()
            .write_all(json.as_jsonl())
            .map_err(write_error)?;
        self.num_records += batch.num_records() as u64;
        Ok(SinkControl::Continue)
    }

    fn finish(self, _summary: &PartitionSummary) -> Result<Self::Output, SinkError> {
        self.writer.lock().flush().map_err(write_error)?;
        Ok(self.num_records)
    }
}

/// # Returns
///
/// The sink error of a failed write: [`SinkError::DownstreamClosed`] for a broken pipe or a
/// connection the peer reset or aborted, else [`SinkError::Io`].
pub fn write_error(error: std::io::Error) -> SinkError {
    match error.kind() {
        std::io::ErrorKind::BrokenPipe
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::ConnectionAborted => SinkError::DownstreamClosed,
        _ => SinkError::Io(error),
    }
}
