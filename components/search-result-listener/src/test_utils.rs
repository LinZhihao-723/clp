//! Utilities for testing consumers of a session's results outside this crate.

use bytes::BytesMut;
use clp_rust_utils::types::ArchiveId;

use crate::ArrowResultBatch;
use crate::arrow_stream::ArrowRead;
use crate::arrow_stream::ArrowStreamReader;

/// Reads a protocol-v3 connection's Arrow IPC stream the way the connection does.
///
/// # Returns
///
/// The record batches of results that the stream carries, validated against the archive
/// `archive_id`.
///
/// # Panics
///
/// Panics if the stream is malformed, or ends before its end-of-stream marker.
#[must_use]
pub fn read_arrow_stream(archive_id: ArchiveId, stream: &[u8]) -> Vec<ArrowResultBatch> {
    let mut reader = ArrowStreamReader::new(archive_id);
    let mut buffer = BytesMut::from(stream);
    let mut batches = Vec::new();
    while !reader.has_ended() {
        match reader
            .read(&mut buffer)
            .expect("the Arrow IPC stream should be well-formed")
        {
            ArrowRead::Batch(batch) => batches.push(batch),
            ArrowRead::Continue => {}
            ArrowRead::Incomplete(_) => panic!("the Arrow IPC stream should be complete"),
        }
    }
    batches
}
