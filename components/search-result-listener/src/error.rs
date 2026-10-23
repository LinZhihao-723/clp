//! Error types for the search result listener.

use std::time::Duration;

use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::job_config::SessionToken;
use clp_rust_utils::task_io::query::QueryTaskIndex;
use clp_rust_utils::types::ArchiveId;
use clp_rust_utils::types::ParseArchiveIdError;

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

    #[error("bytes followed the trailer")]
    DataAfterTrailer,

    #[error("invalid archive ID: {0}")]
    InvalidArchiveId(#[from] ParseArchiveIdError),

    #[error("invalid session token: {0}")]
    InvalidSessionToken(#[from] uuid::Error),

    #[error("malformed frame: {0}")]
    MalformedFrame(String),

    #[error(
        "the trailer counts {trailer_num_results} results, but the connection streamed \
         {num_results}"
    )]
    ResultCountMismatch {
        trailer_num_results: u64,
        num_results: u64,
    },

    #[error("expected result index {expected} on the connection, got {received}")]
    UnexpectedResultIndex { expected: u64, received: u64 },

    #[error("unknown session token {0}")]
    UnknownSession(SessionToken),

    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u64),
}

/// Reasons a connection from a search task ends without an acknowledgement.
#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error("the connection claimed no result within {0:?} after its job terminated")]
    DrainTimedOut(Duration),

    #[error("no handshake arrived within {0:?}")]
    HandshakeTimeout(Duration),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("the connection closed without a trailer")]
    MissingTrailer,

    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    #[error("the connection closed in the middle of a frame")]
    TruncatedFrame,
}
