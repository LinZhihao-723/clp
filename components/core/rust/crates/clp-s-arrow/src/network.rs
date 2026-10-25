//! The protocol-v3 streaming search sink, which streams results to a search-result listener as an
//! Arrow IPC stream.

use std::sync::Arc;

use arrow_buffer::Buffer;
use clp_s_reader::MergeOrder;
use clp_s_reader::OutputRequirements;
use clp_s_reader::PartialSink;
use clp_s_reader::PartitionInfo;
use clp_s_reader::PartitionSummary;
use clp_s_reader::RecordBatch;
use clp_s_reader::Sink;
use clp_s_reader::SinkControl;
use clp_s_reader::SinkError;
use clp_s_sinks::ListenerConnection;
use clp_s_sinks::NetworkDestination;
use clp_s_wire::StreamProtocolVersion;

use crate::ResultBatchLimits;
use crate::ResultStreamWriter;

/// Streams the results of one search task to a search-result listener with protocol v3.
///
/// The partition of the destination's archive connects when it has its first result, and sends
/// the v3 handshake followed by an Arrow IPC stream of [`crate::RESULT_SCHEMA`]: the schema, one
/// dictionary batch holding the archive ID, and at least one record batch per consumed batch,
/// within the sink's [`ResultBatchLimits`]. When it finishes, it ends the IPC stream, sends the
/// trailer counting the results, shuts down its side of the connection, and succeeds only once the
/// listener's acknowledgement counts every result. A partition without results never connects, a
/// partition dropped without finishing sends no trailer, and a partition opened again (a rerun of
/// the session) streams over a new connection, so the listener drops the results it already has.
#[derive(Debug)]
pub struct ArrowNetworkSink {
    destination: Arc<NetworkDestination>,
    limits: ResultBatchLimits,
}

impl ArrowNetworkSink {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A sink streaming to `destination` in record batches within `limits`.
    #[must_use]
    pub fn new(destination: NetworkDestination, limits: ResultBatchLimits) -> Self {
        Self {
            destination: Arc::new(destination),
            limits,
        }
    }
}

impl Sink for ArrowNetworkSink {
    type Output = u64;
    type Partial = ArrowNetworkPartial;

    fn requirements(&self) -> OutputRequirements {
        OutputRequirements::json().with_timestamps()
    }

    fn merge_order(&self) -> MergeOrder {
        MergeOrder::Scan
    }

    fn open_partial(&self, partition: &PartitionInfo) -> Result<Self::Partial, SinkError> {
        self.destination.check_partition(partition)?;
        Ok(ArrowNetworkPartial {
            writer: ResultStreamWriter::new(self.destination.archive_id.as_str(), self.limits),
            destination: Arc::clone(&self.destination),
            connection: None,
            buffers: Vec::new(),
            num_records: 0,
        })
    }

    fn merge(&self, output: &mut Self::Output, partial: u64) -> Result<SinkControl, SinkError> {
        *output += partial;
        Ok(SinkControl::Continue)
    }
}

/// The partial sink of [`ArrowNetworkSink`].
pub struct ArrowNetworkPartial {
    destination: Arc<NetworkDestination>,
    writer: ResultStreamWriter,

    /// The connection to the listener, opened with the first result.
    connection: Option<ListenerConnection>,

    /// The IPC messages of the batch being consumed.
    buffers: Vec<Buffer>,

    /// The number of results streamed.
    num_records: u64,
}

impl std::fmt::Debug for ArrowNetworkPartial {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ArrowNetworkPartial")
            .field("destination", &self.destination)
            .field("connection", &self.connection)
            .field("num_records", &self.num_records)
            .finish_non_exhaustive()
    }
}

impl PartialSink for ArrowNetworkPartial {
    type Output = u64;

    fn consume(&mut self, batch: &RecordBatch<'_>) -> Result<SinkControl, SinkError> {
        if 0 == batch.num_records() {
            return Ok(SinkControl::Continue);
        }
        let missing = |what: &str| SinkError::Encode(format!("the batch has no {what}").into());
        let json = batch.json().ok_or_else(|| missing("rendered records"))?;
        let timestamps_ns = batch.timestamps_ns().ok_or_else(|| missing("timestamps"))?;
        self.buffers.clear();
        self.writer
            .write(json, timestamps_ns, &mut self.buffers)
            .map_err(|error| SinkError::Encode(error.to_string().into()))?;

        let mut handshake = Vec::new();
        if self.connection.is_none() {
            self.destination
                .handshake(StreamProtocolVersion::V3)
                .encode(&mut handshake)
                .map_err(|error| SinkError::Encode(error.to_string().into()))?;
        }
        let connection = if let Some(connection) = &mut self.connection {
            connection
        } else {
            self.connection
                .insert(ListenerConnection::connect(&self.destination)?)
        };
        let mut slices = Vec::with_capacity(self.buffers.len() + 1);
        slices.push(handshake.as_slice());
        slices.extend(self.buffers.iter().map(Buffer::as_slice));
        connection.write_all_vectored(&slices)?;
        self.num_records += batch.num_records() as u64;
        Ok(SinkControl::Continue)
    }

    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`SinkError::Encode`] if [`ResultStreamWriter::finish`] fails.
    /// * Forwards [`ListenerConnection::finish`]'s return values on failure.
    fn finish(self, _summary: &PartitionSummary) -> Result<Self::Output, SinkError> {
        let Some(connection) = self.connection else {
            return Ok(self.num_records);
        };
        let end_of_stream: Vec<u8> = self
            .writer
            .finish()
            .map_err(|error| SinkError::Encode(error.to_string().into()))?
            .iter()
            .flat_map(|buffer| buffer.as_slice().iter().copied())
            .collect();
        connection.finish(&end_of_stream, self.num_records)?;
        Ok(self.num_records)
    }
}
