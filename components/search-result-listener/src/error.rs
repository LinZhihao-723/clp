//! Error types for the search result listener.

use std::time::Duration;

use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::task_io::query::QueryTaskIndex;
use clp_rust_utils::types::ArchiveId;
use clp_rust_utils::types::ParseArchiveIdError;

use crate::SessionToken;

/// Errors returned by the search result listener.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("no IPv4 address found to advertise to the search tasks")]
    NoIpv4Address,

    #[error("query job {0} not found")]
    QueryJobNotFound(QueryJobId),

    #[error("the session task failed: {0}")]
    SessionTask(#[from] tokio::task::JoinError),

    #[error("sqlx error: {0}")]
    Sqlx(#[from] sqlx::Error),
}

/// Violations of the wire protocol, each of which makes the listener close the connection.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error(
        "task {task_index} is bound to archive {expected}, but the connection streams archive \
         {received}"
    )]
    ArchiveIdMismatch {
        task_index: QueryTaskIndex,
        expected: ArchiveId,
        received: ArchiveId,
    },

    #[error("result index {received} is above the task's next unclaimed result index {expected}")]
    IndexGap { expected: u64, received: u64 },

    #[error("invalid archive ID: {0}")]
    InvalidArchiveId(#[from] ParseArchiveIdError),

    #[error("invalid session token: {0}")]
    InvalidSessionToken(#[from] uuid::Error),

    #[error("malformed frame: {0}")]
    MalformedFrame(String),

    #[error("unknown session token {0}")]
    UnknownSession(SessionToken),

    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u64),
}

/// Reasons a connection from a search task ends abnormally.
#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error("no handshake arrived within {0:?}")]
    HandshakeTimeout(Duration),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    #[error("the connection closed in the middle of a frame")]
    TruncatedFrame,
}
