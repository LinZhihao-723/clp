//! Encodings of the results a session delivers, each specific to the transport that carries them to
//! the session's consumer.

use bytes::Bytes;
use clp_rust_utils::types::ArchiveId;

use crate::ArrowResultBatch;

/// A transport-specific encoding of the results a session delivers to its consumer.
///
/// The session's connections encode the results they claim, so that the consumer only forwards
/// batches that are ready for its transport.
pub trait ResultEncoding: Send + Sync + 'static {
    /// The results a connection claimed from one socket read, encoded for the transport.
    type Batch: EncodedBatch;

    /// The encoder of one connection's results.
    type Encoder: ResultEncoder<Batch = Self::Batch>;

    /// Factory function.
    ///
    /// Creates the encoder of a connection that streams the results of one archive.
    ///
    /// # Parameters
    ///
    /// * `archive_id` - The ID of the archive whose results the connection streams.
    ///
    /// # Returns
    ///
    /// The newly created encoder.
    fn encoder(&self, archive_id: ArchiveId) -> Self::Encoder;
}

/// Encodes the results one connection claims into batches.
pub trait ResultEncoder: Send + 'static {
    /// The encoded results of one batch.
    type Batch: Send + 'static;

    /// Appends a claimed result to the pending batch.
    ///
    /// # Parameters
    ///
    /// * `timestamp` - The result's timestamp.
    /// * `message` - The result's log message, without its trailing newline.
    fn encode(&mut self, timestamp: i64, message: &str);

    /// Appends the claimed results of a record batch that a protocol-v3 connection streamed to the
    /// pending batch.
    ///
    /// By default, appends each claimed result with [`ResultEncoder::encode`].
    ///
    /// # Parameters
    ///
    /// * `batch` - The record batch, already validated.
    /// * `num_duplicates` - The number of leading results of the batch that another attempt of the
    ///   task already emitted, which must not be appended. It is below the batch's number of rows.
    fn encode_arrow(&mut self, batch: &ArrowResultBatch, num_duplicates: usize) {
        let timestamps = batch.timestamps();
        let messages = batch.messages();
        for row in num_duplicates..batch.num_rows() {
            self.encode(timestamps.value(row), messages.value(row));
        }
    }

    /// Takes the pending batch, leaving the encoder with an empty one.
    ///
    /// # Returns
    ///
    /// The results appended since the previous call, which number at least one.
    fn take_batch(&mut self) -> Self::Batch;
}

/// A batch of encoded results, whose size bounds how many batches a session buffers for its
/// consumer.
pub trait EncodedBatch: Send + 'static {
    /// # Returns
    ///
    /// The approximate number of bytes the batch occupies.
    fn size(&self) -> usize;
}

impl EncodedBatch for Bytes {
    fn size(&self) -> usize {
        self.len()
    }
}

impl EncodedBatch for Vec<Bytes> {
    fn size(&self) -> usize {
        self.iter().map(Bytes::len).sum()
    }
}

impl EncodedBatch for Vec<SearchResult> {
    fn size(&self) -> usize {
        self.iter()
            .map(|result| size_of::<SearchResult>() + result.message.len())
            .sum()
    }
}

/// A search result emitted by a session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchResult {
    pub archive_id: ArchiveId,
    pub timestamp: i64,

    /// The result's log message, without its trailing newline.
    pub message: String,
}

/// A [`ResultEncoding`] that delivers each batch as the [`SearchResult`]s it holds.
#[derive(Clone, Copy, Debug, Default)]
pub struct SearchResultEncoding;

impl ResultEncoding for SearchResultEncoding {
    type Batch = Vec<SearchResult>;
    type Encoder = SearchResultEncoder;

    fn encoder(&self, archive_id: ArchiveId) -> SearchResultEncoder {
        SearchResultEncoder {
            archive_id,
            batch: Vec::new(),
        }
    }
}

/// The [`ResultEncoder`] of [`SearchResultEncoding`].
pub struct SearchResultEncoder {
    archive_id: ArchiveId,
    batch: Vec<SearchResult>,
}

impl ResultEncoder for SearchResultEncoder {
    type Batch = Vec<SearchResult>;

    fn encode(&mut self, timestamp: i64, message: &str) {
        self.batch.push(SearchResult {
            archive_id: self.archive_id,
            timestamp,
            message: message.to_owned(),
        });
    }

    fn take_batch(&mut self) -> Vec<SearchResult> {
        std::mem::take(&mut self.batch)
    }
}

#[cfg(test)]
mod tests {
    use clp_rust_utils::types::ArchiveId;

    use super::ResultEncoder;
    use super::ResultEncoding;
    use super::SearchResult;
    use super::SearchResultEncoding;

    const ARCHIVE_ID: &str = "018e90e5-8b2a-4a61-a2fc-cac799936caf";

    #[test]
    fn search_result_encoder_keeps_the_claim_order_and_starts_each_batch_empty() {
        let archive_id = ARCHIVE_ID.parse::<ArchiveId>().expect("valid archive UUID");
        let search_result = |timestamp: i64, message: &str| SearchResult {
            archive_id,
            timestamp,
            message: message.to_owned(),
        };
        let mut encoder = SearchResultEncoding.encoder(archive_id);

        encoder.encode(2, "b");
        encoder.encode(1, "a");
        let first_batch = encoder.take_batch();
        encoder.encode(3, "c");
        let second_batch = encoder.take_batch();

        assert_eq!(first_batch, [search_result(2, "b"), search_result(1, "a")]);
        assert_eq!(second_batch, [search_result(3, "c")]);
    }
}
