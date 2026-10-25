//! The protocol-v2 streaming search sink (the C++ `network` output), and the connection to a
//! search-result listener that every protocol version shares.

use std::io::IoSlice;
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
use clp_s_wire::StreamProtocolVersion;
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

impl NetworkDestination {
    /// # Returns
    ///
    /// The handshake that opens a stream of protocol `version` to the destination.
    #[must_use]
    pub fn handshake(&self, version: StreamProtocolVersion) -> StreamHandshake<'_> {
        StreamHandshake {
            version,
            session_token: &self.session_token,
            task_index: self.task_index,
            archive_id: self.archive_id.as_str(),
        }
    }

    /// Checks that `partition` is the archive the destination streams.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`SinkError::Custom`] if `partition` isn't the archive of the destination.
    pub fn check_partition(&self, partition: &PartitionInfo) -> Result<(), SinkError> {
        if matches!(
            &partition.kind,
            PartitionKind::Archive(archive_id) if *archive_id == self.archive_id
        ) {
            return Ok(());
        }
        Err(SinkError::Custom(
            format!(
                "the sink streams only archive {}, not the {:?} partition",
                self.archive_id, partition.kind
            )
            .into(),
        ))
    }
}

/// A connection to a search-result listener, which streams the results of one partition after
/// its handshake and ends with a trailer that the listener acknowledges.
#[derive(Debug)]
pub struct ListenerConnection {
    stream: TcpStream,
}

impl ListenerConnection {
    /// Connects to `destination`.
    ///
    /// # Returns
    ///
    /// The newly opened connection on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`TcpStream::connect`]'s return values on failure.
    pub fn connect(destination: &NetworkDestination) -> Result<Self, SinkError> {
        Ok(Self {
            stream: TcpStream::connect((destination.host.as_str(), destination.port))
                .map_err(write_error)?,
        })
    }

    /// Writes every byte of `slices`, in order.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`SinkError::DownstreamClosed`] if the listener closed the connection.
    /// * Forwards [`Write::write_vectored`]'s return values on failure.
    pub fn write_all_vectored(&mut self, slices: &[&[u8]]) -> Result<(), SinkError> {
        let mut io_slices: Vec<IoSlice<'_>> = slices
            .iter()
            .filter(|slice| !slice.is_empty())
            .map(|slice| IoSlice::new(slice))
            .collect();
        let mut remaining = &mut io_slices[..];
        while !remaining.is_empty() {
            match self.stream.write_vectored(remaining) {
                Ok(0) => return Err(SinkError::DownstreamClosed),
                Ok(num_bytes_written) => IoSlice::advance_slices(&mut remaining, num_bytes_written),
                Err(error) if std::io::ErrorKind::Interrupted == error.kind() => {}
                Err(error) => return Err(write_error(error)),
            }
        }
        Ok(())
    }

    /// Ends the stream: writes `end_of_results` and the [`StreamTrailer`] counting `num_results`
    /// in one write, shuts down the sending side of the connection, and waits for the listener's
    /// [`StreamAck`].
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`SinkError::DownstreamClosed`] if the listener closes or resets the connection before
    ///   acknowledging the stream.
    /// * [`SinkError::Custom`] if the acknowledgement is malformed or counts another number of
    ///   results.
    /// * [`SinkError::Encode`] if [`StreamTrailer::encode`] fails.
    /// * Forwards [`Self::write_all_vectored`]'s return values on failure.
    /// * Forwards [`TcpStream::shutdown`]'s return values on failure.
    /// * Forwards [`Read::read_exact`]'s return values on failure.
    pub fn finish(mut self, end_of_results: &[u8], num_results: u64) -> Result<(), SinkError> {
        let mut trailer = Vec::new();
        StreamTrailer { num_results }
            .encode(&mut trailer)
            .map_err(|error| SinkError::Encode(error.to_string().into()))?;
        self.write_all_vectored(&[end_of_results, &trailer])?;
        self.stream.shutdown(Shutdown::Write).map_err(write_error)?;

        let mut ack = [0; StreamAck::ENCODED_LEN];
        self.stream.read_exact(&mut ack).map_err(|error| {
            if std::io::ErrorKind::UnexpectedEof == error.kind() {
                SinkError::DownstreamClosed
            } else {
                write_error(error)
            }
        })?;
        let ack = StreamAck::decode(&ack).map_err(|error| SinkError::Custom(Box::new(error)))?;
        if num_results != ack.num_results {
            return Err(SinkError::Custom(
                format!(
                    "the listener acknowledged {} results, but the stream sent {num_results}",
                    ack.num_results
                )
                .into(),
            ));
        }
        Ok(())
    }
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
        self.destination.check_partition(partition)?;
        Ok(NetworkPartial {
            destination: Arc::clone(&self.destination),
            connection: None,
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
    connection: Option<ListenerConnection>,

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
        if self.connection.is_none() {
            self.destination
                .handshake(StreamProtocolVersion::V2)
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
        let connection = if let Some(connection) = &mut self.connection {
            connection
        } else {
            self.connection
                .insert(ListenerConnection::connect(&self.destination)?)
        };
        connection.write_all_vectored(&[&self.buffer])?;
        self.num_records = result_index;
        Ok(SinkControl::Continue)
    }

    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`ListenerConnection::finish`]'s return values on failure.
    fn finish(self, _summary: &PartitionSummary) -> Result<Self::Output, SinkError> {
        if let Some(connection) = self.connection {
            connection.finish(&[], self.num_records)?;
        }
        Ok(self.num_records)
    }
}
