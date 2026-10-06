//! Serving a connection from a search task: its handshake, then the results it streams.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_util::task::task_tracker::TaskTrackerToken;

use crate::SearchResult;
use crate::SessionStats;
use crate::cursor::Claim;
use crate::cursor::TaskCursor;
use crate::error::ConnectionError;
use crate::error::ProtocolError;
use crate::listener::PendingHandshake;
use crate::protocol::Handshake;
use crate::protocol::ResultFrame;
use crate::session;

/// Serves a connection from a search task until it ends, and logs how it ended.
///
/// `pending_handshake` is released once the connection has looked up its session.
pub async fn serve(
    mut stream: TcpStream,
    peer_addr: SocketAddr,
    sessions: Arc<session::Registry>,
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
    match end {
        Ok(End::Eof) => {}
        Ok(End::DrainTimedOut) => tracing::debug!(
            peer_addr = % peer_addr,
            session_token = % handshake.session_token,
            task_index = handshake.task_index,
            "Stopped reading a connection that claimed no result within the grace period after \
             its job terminated."
        ),
        Err(e) => {
            if matches!(e, ConnectionError::Protocol(_)) {
                connection.stats.num_protocol_errors += 1;
            }
            tracing::warn!(
                peer_addr = % peer_addr,
                session_token = % handshake.session_token,
                task_index = handshake.task_index,
                error = % e,
                "Connection ended abnormally."
            );
        }
    }
}

/// The number of bytes the read buffer has room for before each socket read.
const READ_BUFFER_CAPACITY: usize = 64 * 1024;

/// How a connection that joined a session ended normally.
enum End {
    /// The search task closed the connection after its last result.
    Eof,

    /// The session's job terminated, and the connection then waited on its socket for the grace
    /// period without claiming a result.
    DrainTimedOut,
}

/// A connection that joined a session, streaming the results of one attempt of a task.
struct Connection {
    session: Arc<session::State>,
    cursor: Arc<TaskCursor>,
    stats: SessionStats,

    /// Declared last so that it is dropped last: the session's drain completes only after this
    /// connection has recorded its statistics and released the session.
    _session_connection_token: TaskTrackerToken,
}

impl Connection {
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
    fn join(sessions: &session::Registry, handshake: &Handshake) -> Result<Self, ProtocolError> {
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
            session,
            cursor,
            stats: SessionStats::default(),
            _session_connection_token: session_connection_token,
        })
    }

    /// Claims each result the connection streams, and sends the claimed results to the session's
    /// consumer, or discards them once the consumer has dropped the result stream.
    ///
    /// Reading stops at EOF. Once the session's job has terminated, it also stops when the
    /// connection has waited on its socket for the session's grace period without claiming a
    /// result. Delivering a claimed result is never cut short, and doesn't count toward the grace
    /// period.
    ///
    /// # Returns
    ///
    /// How the connection ended on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ProtocolError::ArchiveIdMismatch`] if the cursor of the handshake's task is bound to
    ///   another archive.
    /// * [`ConnectionError::TruncatedFrame`] if the connection closes in the middle of a frame.
    /// * Forwards [`ResultFrame::decode`]'s return values on failure.
    /// * Forwards [`TaskCursor::try_claim`]'s return values on failure.
    /// * Forwards [`AsyncReadExt::read_buf`]'s return values on failure.
    async fn run(
        &mut self,
        handshake: &Handshake,
        stream: &mut TcpStream,
        mut buffer: BytesMut,
    ) -> Result<End, ConnectionError> {
        let archive_id = self.cursor.archive_id();
        if archive_id != handshake.archive_id {
            return Err(ProtocolError::ArchiveIdMismatch {
                task_index: handshake.task_index,
                expected: archive_id,
                received: handshake.archive_id,
            }
            .into());
        }

        let mut read_time_since_last_claim = Duration::ZERO;
        loop {
            while let Some(frame) = ResultFrame::decode(&mut buffer)? {
                if Claim::Duplicate == self.cursor.try_claim(frame.result_index)? {
                    self.stats.num_duplicates_dropped += 1;
                    continue;
                }
                read_time_since_last_claim = Duration::ZERO;
                if self.session.record_claimed_result() {
                    tracing::debug!(
                        session_token = % handshake.session_token,
                        task_index = handshake.task_index,
                        "Claimed the session's first result."
                    );
                }
                let result = SearchResult {
                    archive_id,
                    timestamp: frame.timestamp,
                    message: frame.message,
                };
                if self.session.results_sender.send(result).await.is_ok() {
                    self.stats.num_results_emitted += 1;
                } else {
                    self.stats.num_results_discarded += 1;
                }
            }

            buffer.reserve(READ_BUFFER_CAPACITY);
            let num_bytes_read = if self.session.draining.is_cancelled() {
                let read_started_at = Instant::now();
                let Ok(read_result) = tokio::time::timeout(
                    self.session
                        .drain_grace_period
                        .saturating_sub(read_time_since_last_claim),
                    stream.read_buf(&mut buffer),
                )
                .await
                else {
                    return Ok(End::DrainTimedOut);
                };
                read_time_since_last_claim += read_started_at.elapsed();
                read_result?
            } else {
                tokio::select! {
                    biased;
                    read_result = stream.read_buf(&mut buffer) => read_result?,
                    () = self.session.draining.cancelled() => continue,
                }
            };
            if 0 == num_bytes_read {
                if buffer.is_empty() {
                    return Ok(End::Eof);
                }
                return Err(ConnectionError::TruncatedFrame);
            }
        }
    }
}

impl Drop for Connection {
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
