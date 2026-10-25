//! The protocol-v2 streaming search sink (the C++ `network` output).

use std::io::Read;
use std::io::Write;
use std::net::Shutdown;
use std::net::TcpStream;
use std::sync::Arc;

use clp_s_reader::ArchiveId;
use clp_s_reader::MergeOrder;
use clp_s_reader::OutputRequirements;
use clp_s_reader::PartialSink;
use clp_s_reader::PartitionInfo;
use clp_s_reader::PartitionKind;
use clp_s_reader::PartitionSummary;
use clp_s_reader::RecordBatch;
use clp_s_reader::Sink;
use clp_s_reader::SinkControl;
use clp_s_reader::SinkError;
use clp_s_wire::StreamAck;
use clp_s_wire::StreamHandshake;
use clp_s_wire::StreamResult;
use clp_s_wire::StreamTrailer;
use clp_s_wire::WireError;
use clp_s_wire::epoch_ns_to_wire_ms;

use crate::json_lines::write_error;

/// Where a search task streams its results, and how it identifies itself to the listener.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkDestination {
    pub host: String,
    pub port: u16,
    pub session_token: String,
    pub task_index: u64,

    /// The archive the task streams, announced in the handshake.
    pub archive_id: ArchiveId,
}

/// Streams the results of one search task to a search-result listener with protocol v2, like the
/// C++ `network` output.
///
/// The partition of the destination's archive connects when it has its first result, and sends
/// the [`StreamHandshake`] and one [`StreamResult`] per record numbered from 0. When it finishes,
/// it sends the [`StreamTrailer`], shuts down its side of the connection, and succeeds only once
/// the listener's [`StreamAck`] counts every result. A partition without results never connects,
/// and a partition dropped without finishing sends no trailer. A partition that is opened again (a
/// rerun of the session) streams over a new connection numbered from 0 again, so the listener
/// drops the results it already has.
#[derive(Debug)]
pub struct NetworkSink {
    destination: Arc<NetworkDestination>,
}

impl NetworkSink {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A sink streaming to `destination`.
    #[must_use]
    pub fn new(destination: NetworkDestination) -> Self {
        Self {
            destination: Arc::new(destination),
        }
    }
}

impl Sink for NetworkSink {
    type Output = u64;
    type Partial = NetworkPartial;

    fn requirements(&self) -> OutputRequirements {
        OutputRequirements::json().with_timestamps()
    }

    fn merge_order(&self) -> MergeOrder {
        MergeOrder::Scan
    }

    fn open_partial(&self, partition: &PartitionInfo) -> Result<Self::Partial, SinkError> {
        if !matches!(
            &partition.kind,
            PartitionKind::Archive(archive_id) if *archive_id == self.destination.archive_id
        ) {
            return Err(SinkError::Custom(
                format!(
                    "the sink streams only archive {}, not the {:?} partition",
                    self.destination.archive_id, partition.kind
                )
                .into(),
            ));
        }
        Ok(NetworkPartial {
            destination: Arc::clone(&self.destination),
            stream: None,
            buffer: Vec::new(),
            num_records: 0,
        })
    }

    fn merge(&self, output: &mut Self::Output, partial: u64) -> Result<SinkControl, SinkError> {
        *output += partial;
        Ok(SinkControl::Continue)
    }
}

/// The partial sink of [`NetworkSink`].
#[derive(Debug)]
pub struct NetworkPartial {
    destination: Arc<NetworkDestination>,

    /// The connection to the listener, opened with the first result.
    stream: Option<TcpStream>,

    /// The encoded frames of the batch being consumed.
    buffer: Vec<u8>,

    /// The number of results streamed, which is also the index of the next result.
    num_records: u64,
}

impl PartialSink for NetworkPartial {
    type Output = u64;

    fn consume(&mut self, batch: &RecordBatch<'_>) -> Result<SinkControl, SinkError> {
        if 0 == batch.num_records() {
            return Ok(SinkControl::Continue);
        }
        let missing = |what: &str| SinkError::Encode(format!("the batch has no {what}").into());
        let encode_error = |error: WireError| SinkError::Encode(error.to_string().into());
        let json = batch.json().ok_or_else(|| missing("rendered records"))?;
        let timestamps_ns = batch.timestamps_ns().ok_or_else(|| missing("timestamps"))?;
        self.buffer.clear();
        if self.stream.is_none() {
            StreamHandshake {
                session_token: &self.destination.session_token,
                task_index: self.destination.task_index,
                archive_id: self.destination.archive_id.as_str(),
            }
            .encode(&mut self.buffer)
            .map_err(encode_error)?;
        }
        let mut result_index = self.num_records;
        for (record_index, timestamp_ns) in timestamps_ns.iter().enumerate() {
            StreamResult {
                result_index,
                timestamp_ms: timestamp_ns.map_or(0, epoch_ns_to_wire_ms),
                message: json
                    .record(record_index)
                    .ok_or_else(|| missing(&format!("rendered text for record {record_index}")))?,
            }
            .encode(&mut self.buffer)
            .map_err(encode_error)?;
            result_index += 1;
        }
        let stream = if let Some(stream) = &mut self.stream {
            stream
        } else {
            self.stream.insert(
                TcpStream::connect((self.destination.host.as_str(), self.destination.port))
                    .map_err(write_error)?,
            )
        };
        stream.write_all(&self.buffer).map_err(write_error)?;
        self.num_records = result_index;
        Ok(SinkControl::Continue)
    }

    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`SinkError::DownstreamClosed`] if the listener closes or resets the connection before
    ///   acknowledging the stream.
    /// * [`SinkError::Custom`] if the acknowledgement is malformed or counts another number of
    ///   results.
    /// * Forwards [`StreamTrailer::encode`]'s return values on failure.
    /// * Forwards [`Write::write_all`]'s return values on failure.
    /// * Forwards [`TcpStream::shutdown`]'s return values on failure.
    /// * Forwards [`Read::read_exact`]'s return values on failure.
    fn finish(self, _summary: &PartitionSummary) -> Result<Self::Output, SinkError> {
        let Some(mut stream) = self.stream else {
            return Ok(self.num_records);
        };
        let mut trailer = Vec::new();
        StreamTrailer {
            num_results: self.num_records,
        }
        .encode(&mut trailer)
        .map_err(|error| SinkError::Encode(error.to_string().into()))?;
        stream.write_all(&trailer).map_err(write_error)?;
        stream.shutdown(Shutdown::Write).map_err(write_error)?;

        let mut ack = [0; StreamAck::ENCODED_LEN];
        stream.read_exact(&mut ack).map_err(|error| {
            if std::io::ErrorKind::UnexpectedEof == error.kind() {
                SinkError::DownstreamClosed
            } else {
                write_error(error)
            }
        })?;
        let ack = StreamAck::decode(&ack).map_err(|error| SinkError::Custom(Box::new(error)))?;
        if self.num_records != ack.num_results {
            return Err(SinkError::Custom(
                format!(
                    "the listener acknowledged {} results, but the stream sent {}",
                    ack.num_results, self.num_records
                )
                .into(),
            ));
        }
        Ok(self.num_records)
    }
}
