//! Serving a connection from a search task: its handshake, then the results it streams as msgpack
//! frames (protocol v2) or as an Arrow IPC stream (protocol v3), then the trailer that the
//! connection's acknowledgement answers.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::BufMut;
use bytes::BytesMut;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_util::task::task_tracker::TaskTrackerToken;

use crate::ResultEncoder;
use crate::ResultEncoding;
use crate::SessionStats;
use crate::arrow_stream::ArrowRead;
use crate::arrow_stream::ArrowResultBatch;
use crate::arrow_stream::ArrowStreamReader;
use crate::cursor::Claim;
use crate::cursor::TaskCursor;
use crate::error::ConnectionError;
use crate::error::ProtocolError;
use crate::listener::PendingHandshake;
use crate::protocol::Ack;
use crate::protocol::Frame;
use crate::protocol::Handshake;
use crate::protocol::ProtocolVersion;
use crate::protocol::ResultFrame;
use crate::protocol::Trailer;
use crate::session;

/// Serves a connection from a search task until it ends, and logs how it ended.
///
/// `pending_handshake` is released once the connection has looked up its session.
///
/// # Type Parameters
///
/// * `EncodingType` - The encoding of the results the sessions deliver.
pub async fn serve<EncodingType: ResultEncoding>(
    mut stream: TcpStream,
    peer_addr: SocketAddr,
    sessions: Arc<session::Registry<EncodingType>>,
    handshake_timeout: Duration,
    pending_handshake: PendingHandshake,
) {
    let mut buffer = BytesMut::with_capacity(READ_BUFFER_CAPACITY);
    let handshake =
        match tokio::time::timeout(handshake_timeout, read_handshake(&mut stream, &mut buffer))
            .await
            .unwrap_or(Err(ConnectionError::HandshakeTimeout(handshake_timeout)))
        {
            Ok(Some(handshake)) => handshake,
            Ok(None) => {
                tracing::debug!(
                    peer_addr = % peer_addr,
                    "Connection closed before sending a handshake."
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    peer_addr = % peer_addr,
                    error = % e,
                    "Closing a connection without a valid handshake."
                );
                return;
            }
        };

    let joined = Connection::join(&sessions, &handshake);
    drop(pending_handshake);
    let mut connection = match joined {
        Ok(connection) => connection,
        Err(e) => {
            tracing::warn!(
                peer_addr = % peer_addr,
                task_index = handshake.task_index,
                error = % e,
                "Rejecting a connection."
            );
            return;
        }
    };
    if connection.session.record_joined_connection() {
        tracing::debug!(
            peer_addr = % peer_addr,
            session_token = % handshake.session_token,
            task_index = handshake.task_index,
            "Accepted the session's first connection."
        );
    }

    let end = connection.run(&handshake, &mut stream, buffer).await;
    if 0 != connection.stats.num_results_discarded {
        tracing::debug!(
            peer_addr = % peer_addr,
            session_token = % handshake.session_token,
            task_index = handshake.task_index,
            num_results_discarded = connection.stats.num_results_discarded,
            "Discarded the results of a connection since the session's result stream was dropped."
        );
    }
    let Err(e) = end else {
        return;
    };
    if matches!(e, ConnectionError::Protocol(_)) {
        connection.stats.num_protocol_errors += 1;
    } else {
        connection.stats.num_incomplete_connections += 1;
    }
    tracing::warn!(
        peer_addr = % peer_addr,
        session_token = % handshake.session_token,
        task_index = handshake.task_index,
        error = % e,
        "Closing a connection without acknowledging its results."
    );
}

/// The most bytes a connection reads from its socket at once while streaming results, which bounds
/// the results in one batch, unless the IPC message being read needs more.
const READ_BUFFER_CAPACITY: usize = 64 * 1024;

/// A connection that joined a session, streaming the results of one attempt of a task.
///
/// # Type Parameters
///
/// * `EncodingType` - The encoding of the results the session delivers.
struct Connection<EncodingType: ResultEncoding> {
    session: Arc<session::State<EncodingType>>,
    cursor: Arc<TaskCursor>,
    encoder: EncodingType::Encoder,

    /// The Arrow IPC stream of a protocol-v3 connection, or `None` for a protocol-v2 connection.
    arrow_stream: Option<ArrowStreamReader>,

    num_pending_results: u64,

    /// The number of results the connection streamed, claimed or not, which is also the index of
    /// the next result it must stream.
    num_results: u64,

    /// The trailer, once the connection has sent it.
    trailer: Option<Trailer>,

    stats: SessionStats,

    /// Declared last so that it is dropped last: the session's drain completes only after this
    /// connection has recorded its statistics and released the session.
    _session_connection_token: TaskTrackerToken,
}

impl<EncodingType: ResultEncoding> Connection<EncodingType> {
    /// Joins the connection to the session and the task cursor that `handshake` names, creating
    /// the cursor if this is the task's first connection.
    ///
    /// # Returns
    ///
    /// The joined connection on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ProtocolError::UnknownSession`] if no registered session has the handshake's token.
    fn join(
        sessions: &session::Registry<EncodingType>,
        handshake: &Handshake,
    ) -> Result<Self, ProtocolError> {
        // The connection token is taken while the registry entry is locked. A session unregisters
        // itself before it drains, so its drain can't miss a connection that found it.
        let (session, session_connection_token) = sessions
            .get(&handshake.session_token)
            .map(|entry| (Arc::clone(entry.value()), entry.value().connections.token()))
            .ok_or(ProtocolError::UnknownSession(handshake.session_token))?;
        let cursor = Arc::clone(
            session
                .cursors
                .entry(handshake.task_index)
                .or_insert_with(|| Arc::new(TaskCursor::new(handshake.archive_id)))
                .value(),
        );
        Ok(Self {
            encoder: session.encoding.encoder(cursor.archive_id()),
            arrow_stream: match handshake.version {
                ProtocolVersion::V2 => None,
                ProtocolVersion::V3 => Some(ArrowStreamReader::new(cursor.archive_id())),
            },
            session,
            cursor,
            num_pending_results: 0,
            num_results: 0,
            trailer: None,
            stats: SessionStats::default(),
            _session_connection_token: session_connection_token,
        })
    }

    /// Claims the results the connection streams, each record batch of a protocol-v3 connection as
    /// one range, encodes the results claimed from each socket read into one batch, and sends the
    /// batch to the session's consumer, or discards it once the consumer has dropped the result
    /// stream. Once the connection has sent its trailer and EOF, and its last batch has been
    /// sent or discarded, acknowledges its results.
    ///
    /// Once the session's job has terminated, reading also stops when the connection has waited
    /// on its socket for the session's drain grace period without claiming a result. Delivering a
    /// claimed batch and writing the acknowledgement are never cut short, and don't count toward
    /// the grace period.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ProtocolError::ArchiveIdMismatch`] if the cursor of the handshake's task is bound to
    ///   another archive.
    /// * [`ConnectionError::DrainTimedOut`] if the connection waits on its socket for the grace
    ///   period after the session's job has terminated.
    /// * [`ConnectionError::MissingTrailer`] if the connection closes between two frames before
    ///   sending its trailer.
    /// * [`ConnectionError::TruncatedFrame`] if the connection closes in the middle of a frame.
    /// * Forwards [`Connection::claim_buffered_frames`]'s return values on failure.
    /// * Forwards [`AsyncReadExt::read_buf`]'s return values on failure.
    /// * Forwards [`AsyncWriteExt::write_all`]'s return values on failure.
    async fn run(
        &mut self,
        handshake: &Handshake,
        stream: &mut TcpStream,
        mut buffer: BytesMut,
    ) -> Result<(), ConnectionError> {
        let archive_id = self.cursor.archive_id();
        if archive_id != handshake.archive_id {
            return Err(ProtocolError::ArchiveIdMismatch {
                task_index: handshake.task_index,
                expected: archive_id,
                received: handshake.archive_id,
            }
            .into());
        }
        if self.arrow_stream.is_some() {
            buffer = BytesMut::from(&buffer[..]);
        }

        let mut read_time_since_last_claim = Duration::ZERO;
        loop {
            let claimed = self.claim_buffered_frames(handshake, &mut buffer);
            if 0 != self.num_pending_results {
                read_time_since_last_claim = Duration::ZERO;
                self.send_pending_batch().await;
            }
            let read_size = claimed?
                .saturating_sub(buffer.len())
                .max(READ_BUFFER_CAPACITY);

            buffer.reserve(read_size);
            let mut read_window = (&mut buffer).limit(read_size);
            let num_bytes_read = if self.session.draining.is_cancelled() {
                let drain_grace_period = self.session.drain_grace_period();
                let read_started_at = Instant::now();
                let Ok(read_result) = tokio::time::timeout(
                    drain_grace_period.saturating_sub(read_time_since_last_claim),
                    stream.read_buf(&mut read_window),
                )
                .await
                else {
                    return Err(ConnectionError::DrainTimedOut(drain_grace_period));
                };
                read_time_since_last_claim += read_started_at.elapsed();
                read_result?
            } else {
                tokio::select! {
                    biased;
                    read_result = stream.read_buf(&mut read_window) => read_result?,
                    () = self.session.draining.cancelled() => continue,
                }
            };
            if 0 != num_bytes_read {
                continue;
            }
            if let Some(trailer) = self.trailer {
                stream
                    .write_all(
                        &Ack {
                            num_results: trailer.num_results,
                        }
                        .encode(),
                    )
                    .await?;
                return Ok(());
            }
            if buffer.is_empty() {
                return Err(ConnectionError::MissingTrailer);
            }
            return Err(ConnectionError::TruncatedFrame);
        }
    }

    /// Decodes each complete frame in `buffer`, encoding the claimed results into the pending
    /// batch, and checks the trailer against the results the connection streamed.
    ///
    /// # Returns
    ///
    /// The number of bytes `buffer` must hold for the next IPC message of a protocol-v3 connection
    /// to be complete, or 0 if unknown, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ProtocolError::DataAfterTrailer`] if any byte follows the trailer.
    /// * [`ProtocolError::ResultCountMismatch`] if the trailer doesn't count the results the
    ///   connection streamed.
    /// * [`ProtocolError::MalformedArrowStream`] if a protocol-v3 connection sends a result frame
    ///   after its Arrow IPC stream.
    /// * Forwards [`ArrowStreamReader::read`]'s return values on failure.
    /// * Forwards [`Frame::decode`]'s return values on failure.
    /// * Forwards [`Connection::claim`]'s return values on failure.
    fn claim_buffered_frames(
        &mut self,
        handshake: &Handshake,
        buffer: &mut BytesMut,
    ) -> Result<usize, ProtocolError> {
        loop {
            if self.trailer.is_some() {
                if buffer.is_empty() {
                    return Ok(0);
                }
                return Err(ProtocolError::DataAfterTrailer);
            }
            if let Some(arrow_stream) = self.arrow_stream.as_mut()
                && !arrow_stream.has_ended()
            {
                match arrow_stream.read(buffer)? {
                    ArrowRead::Batch(batch) => self.claim_arrow_batch(handshake, &batch),
                    ArrowRead::Continue => {}
                    ArrowRead::Incomplete(num_bytes_needed) => return Ok(num_bytes_needed),
                }
                continue;
            }
            let Some(frame) = Frame::decode(buffer)? else {
                return Ok(0);
            };
            match frame {
                Frame::Result(_) if self.arrow_stream.is_some() => {
                    return Err(ProtocolError::MalformedArrowStream(
                        "a result frame followed the Arrow IPC stream".to_owned(),
                    ));
                }
                Frame::Result(result) => self.claim(handshake, &result)?,
                Frame::Trailer(trailer) => {
                    if trailer.num_results != self.num_results {
                        return Err(ProtocolError::ResultCountMismatch {
                            trailer_num_results: trailer.num_results,
                            num_results: self.num_results,
                        });
                    }
                    self.trailer = Some(trailer);
                }
            }
        }
    }

    /// Claims `result` on the task's cursor, encoding it into the pending batch unless an attempt
    /// of the task already emitted it.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ProtocolError::UnexpectedResultIndex`] if `result` isn't the connection's next result.
    fn claim(&mut self, handshake: &Handshake, result: &ResultFrame) -> Result<(), ProtocolError> {
        if result.result_index != self.num_results {
            return Err(ProtocolError::UnexpectedResultIndex {
                expected: self.num_results,
                received: result.result_index,
            });
        }
        self.num_results += 1;
        if Claim::Duplicate == self.cursor.claim(result.result_index) {
            self.stats.num_duplicates_dropped += 1;
            return Ok(());
        }
        if self.session.record_claimed_result() {
            tracing::debug!(
                session_token = % handshake.session_token,
                task_index = handshake.task_index,
                "Claimed the session's first result."
            );
        }
        self.encoder.encode(
            result.timestamp,
            result.message.strip_suffix('\n').unwrap_or(&result.message),
        );
        self.num_pending_results += 1;
        Ok(())
    }

    /// Claims the results of `batch`, the connection's next results, on the task's cursor as one
    /// range, encoding the suffix that no attempt of the task has emitted yet into the pending
    /// batch.
    fn claim_arrow_batch(&mut self, handshake: &Handshake, batch: &ArrowResultBatch) {
        let num_rows = batch.num_rows() as u64;
        if 0 == num_rows {
            return;
        }
        let num_duplicates = self.cursor.claim_range(self.num_results, num_rows);
        self.num_results += num_rows;
        self.stats.num_duplicates_dropped += num_duplicates;
        if num_duplicates == num_rows {
            return;
        }
        if self.session.record_claimed_result() {
            tracing::debug!(
                session_token = % handshake.session_token,
                task_index = handshake.task_index,
                "Claimed the session's first result."
            );
        }
        self.encoder.encode_arrow(
            batch,
            usize::try_from(num_duplicates).expect("a batch's duplicates should fit in `usize`"),
        );
        self.num_pending_results += num_rows - num_duplicates;
    }

    /// Sends the pending batch to the session's consumer, or discards it once the consumer has
    /// dropped the result stream.
    async fn send_pending_batch(&mut self) {
        let num_results = std::mem::take(&mut self.num_pending_results);
        if self
            .session
            .batches_sender
            .send(self.encoder.take_batch())
            .await
            .is_ok()
        {
            self.stats.num_results_emitted += num_results;
        } else {
            self.stats.num_results_discarded += num_results;
        }
    }
}

impl<EncodingType: ResultEncoding> Drop for Connection<EncodingType> {
    fn drop(&mut self) {
        self.session.record(&self.stats);
    }
}

/// Reads the handshake a search task sends first on its connection.
///
/// # Returns
///
/// On success:
///
/// * The handshake.
/// * `None` if the connection closes before sending any bytes.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`ConnectionError::TruncatedFrame`] if the connection closes in the middle of the handshake.
/// * Forwards [`Handshake::decode`]'s return values on failure.
/// * Forwards [`AsyncReadExt::read_buf`]'s return values on failure.
async fn read_handshake(
    stream: &mut TcpStream,
    buffer: &mut BytesMut,
) -> Result<Option<Handshake>, ConnectionError> {
    loop {
        if let Some(handshake) = Handshake::decode(buffer)? {
            return Ok(Some(handshake));
        }
        buffer.reserve(READ_BUFFER_CAPACITY);
        if 0 == stream.read_buf(buffer).await? {
            if buffer.is_empty() {
                return Ok(None);
            }
            return Err(ConnectionError::TruncatedFrame);
        }
    }
}
