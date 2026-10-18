//! The results-cache sink.

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
use clp_s_wire::ResultsCacheField;
use clp_s_wire::epoch_ns_to_wire_ms;
use mongodb::bson::Bson;
use mongodb::bson::Document;

use crate::ResultsCacheError;

/// The database handle hosts build and pass in.
pub type MongoDatabase = mongodb::Database;

/// The `MongoDB` error code of a duplicate-key write error.
const DUPLICATE_KEY_ERROR_CODE: i32 = 11_000;

/// Tunables of the results-cache sink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResultsCacheConfig {
    /// Documents per `insert_many` (C++ `--batch-size`, default 1,000).
    pub batch_size: usize,
}

impl Default for ResultsCacheConfig {
    fn default() -> Self {
        Self { batch_size: 1000 }
    }
}

/// Connects to the results cache named by `uri` (which must name the database, like C++).
///
/// The connection itself is established lazily, by the first insert.
///
/// # Returns
///
/// The database on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`ResultsCacheError::CurrentThreadRuntime`] if `runtime` is a current-thread runtime.
/// * [`ResultsCacheError::InvalidUri`] if `uri` cannot be parsed or no client can be built from it.
/// * [`ResultsCacheError::MissingDatabase`] if `uri` names no database.
///
/// # Panics
///
/// Panics if called from within an asynchronous execution context (`block_on` on `runtime`).
pub fn connect_database(
    runtime: &tokio::runtime::Handle,
    uri: &str,
) -> Result<MongoDatabase, ResultsCacheError> {
    check_runtime(runtime)?;
    let options = runtime
        .block_on(async { mongodb::options::ClientOptions::parse(uri).await })
        .map_err(ResultsCacheError::InvalidUri)?;
    let database = options
        .default_database
        .clone()
        .ok_or(ResultsCacheError::MissingDatabase)?;
    let _entered = runtime.enter();
    let client = mongodb::Client::with_options(options).map_err(ResultsCacheError::InvalidUri)?;
    Ok(client.database(&database))
}

/// Inserts every record into one results-cache collection.
///
/// Documents are `{_id: {archive_id, log_event_idx}, orig_file_path: "", message, timestamp,
/// dataset}`, written with unordered `insert_many` batches; duplicate-key errors are tolerated so a
/// retried Spider task is idempotent.
///
/// Under [`clp_s_reader::ResultOrder::LatestFirst`] the engine keeps the session-wide top-k and
/// delivers the winners once, in the top-k partition; the sink inserts what it receives.
#[derive(Debug)]
pub struct ResultsCacheSink {
    runtime: tokio::runtime::Handle,
    database: MongoDatabase,
    collection: Box<str>,
    dataset: Arc<str>,
    config: ResultsCacheConfig,
}

impl ResultsCacheSink {
    /// Factory function.
    ///
    /// The sink blocks on `runtime` for every insert, so the session must not run inside an
    /// asynchronous execution context.
    ///
    /// # Returns
    ///
    /// The sink on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ResultsCacheError::CurrentThreadRuntime`] if `runtime` is a current-thread runtime.
    /// * [`ResultsCacheError::InvalidBatchSize`] if `config.batch_size` is 0.
    pub fn new(
        runtime: tokio::runtime::Handle,
        database: MongoDatabase,
        collection: &str,
        dataset: &str,
        config: ResultsCacheConfig,
    ) -> Result<Self, ResultsCacheError> {
        check_runtime(&runtime)?;
        if 0 == config.batch_size {
            return Err(ResultsCacheError::InvalidBatchSize);
        }
        Ok(Self {
            runtime,
            database,
            collection: Box::from(collection),
            dataset: Arc::from(dataset),
            config,
        })
    }
}

impl Sink for ResultsCacheSink {
    type Output = u64;
    type Partial = ResultsCachePartial;

    fn requirements(&self) -> OutputRequirements {
        OutputRequirements::json().with_timestamps()
    }

    fn merge_order(&self) -> MergeOrder {
        MergeOrder::Commutative
    }

    fn open_partial(&self, _partition: &PartitionInfo) -> Result<Self::Partial, SinkError> {
        Ok(ResultsCachePartial {
            runtime: self.runtime.clone(),
            collection: self.database.collection(&self.collection),
            dataset: Arc::clone(&self.dataset),
            batch_size: self.config.batch_size,
            pending: Vec::new(),
            num_documents: 0,
        })
    }

    fn merge(&self, output: &mut Self::Output, partial: u64) -> Result<SinkControl, SinkError> {
        *output += partial;
        Ok(SinkControl::Continue)
    }
}

/// The partial sink of [`ResultsCacheSink`].
#[derive(Debug)]
pub struct ResultsCachePartial {
    runtime: tokio::runtime::Handle,
    collection: mongodb::Collection<Document>,
    dataset: Arc<str>,
    batch_size: usize,

    /// Documents not inserted yet (fewer than `batch_size`).
    pending: Vec<Document>,

    /// The number of documents inserted (or found already present) so far.
    num_documents: u64,
}

impl ResultsCachePartial {
    /// Inserts the pending documents with one unordered `insert_many`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`SinkError::Database`] if the insert fails with anything but duplicate-key write errors.
    fn insert_pending(&mut self) -> Result<(), SinkError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let documents = std::mem::take(&mut self.pending);
        let num_documents = documents.len() as u64;
        let collection = &self.collection;
        let result = self
            .runtime
            .block_on(async { collection.insert_many(documents).ordered(false).await });
        match result {
            Ok(_) => {}
            Err(error) if is_only_duplicate_keys(&error) => {
                tracing::debug!(
                    error = % error,
                    "Tolerating duplicate-key errors of a results-cache insert."
                );
            }
            Err(error) => return Err(SinkError::Database(Box::new(error))),
        }
        self.num_documents += num_documents;
        Ok(())
    }
}

impl PartialSink for ResultsCachePartial {
    type Output = u64;

    fn consume(&mut self, batch: &RecordBatch<'_>) -> Result<SinkControl, SinkError> {
        let missing = |what: &str| SinkError::Encode(format!("the batch has no {what}").into());
        let json = batch.json().ok_or_else(|| missing("rendered records"))?;
        let timestamps_ns = batch.timestamps_ns().ok_or_else(|| missing("timestamps"))?;
        for (record, record_id) in batch.record_ids().iter().enumerate() {
            let text = json
                .record(record)
                .ok_or_else(|| missing("record text for every record"))?;
            self.pending.push(search_result_document(
                batch.archive_id(record).as_str(),
                record_id.get(),
                text,
                timestamps_ns[record].map_or(0, epoch_ns_to_wire_ms),
                &self.dataset,
            ));
            if self.pending.len() >= self.batch_size {
                self.insert_pending()?;
            }
        }
        Ok(SinkControl::Continue)
    }

    fn finish(mut self, _summary: &PartitionSummary) -> Result<Self::Output, SinkError> {
        self.insert_pending()?;
        Ok(self.num_documents)
    }
}

/// Builds one search-result document in the field order of C++
/// `ResultsCacheOutputHandler::append_result`. The `_id`'s `log_event_idx` is `record_id`: the
/// `log_event_idx` when the archive records log order, else the table-order ordinal (DV-24).
///
/// # Returns
///
/// The document of the record `message` (its JSON text without a newline; invalid UTF-8 is
/// replaced with U+FFFD, DV-45).
fn search_result_document(
    archive_id: &str,
    record_id: i64,
    message: &[u8],
    timestamp_ms: i64,
    dataset: &str,
) -> Document {
    let mut id = Document::new();
    id.insert(ResultsCacheField::ArchiveId.key(), archive_id);
    id.insert(ResultsCacheField::LogEventIdx.key(), Bson::Int64(record_id));
    let mut text = String::from_utf8_lossy(message).into_owned();
    text.push('\n');
    let mut document = Document::new();
    document.insert(ResultsCacheField::Id.key(), id);
    document.insert(ResultsCacheField::OrigFilePath.key(), "");
    document.insert(ResultsCacheField::Message.key(), text);
    document.insert(
        ResultsCacheField::Timestamp.key(),
        Bson::Int64(timestamp_ms),
    );
    document.insert(ResultsCacheField::Dataset.key(), dataset);
    document
}

/// Mirrors C++ `contains_only_duplicate_key_write_errors` (`MongoDBUtils.cpp`), as far as the
/// driver exposes the server reply.
///
/// # Returns
///
/// Whether `error` is an `insert_many` failure made only of duplicate-key write errors.
fn is_only_duplicate_keys(error: &mongodb::error::Error) -> bool {
    let mongodb::error::ErrorKind::InsertMany(failure) = error.kind.as_ref() else {
        return false;
    };
    if failure.write_concern_error.is_some() {
        return false;
    }
    failure.write_errors.as_ref().is_some_and(|write_errors| {
        !write_errors.is_empty()
            && write_errors
                .iter()
                .all(|write_error| DUPLICATE_KEY_ERROR_CODE == write_error.code)
    })
}

/// # Errors
///
/// Returns an error if:
///
/// * [`ResultsCacheError::CurrentThreadRuntime`] if `runtime` is a current-thread runtime.
fn check_runtime(runtime: &tokio::runtime::Handle) -> Result<(), ResultsCacheError> {
    if tokio::runtime::RuntimeFlavor::CurrentThread == runtime.runtime_flavor() {
        return Err(ResultsCacheError::CurrentThreadRuntime);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use mongodb::bson::Bson;
    use mongodb::bson::doc;

    use super::ResultsCacheConfig;
    use super::search_result_document;

    #[test]
    fn documents_have_the_cpp_shape_and_field_order() {
        let document = search_result_document("a.clps", 7, b"{\"a\":1}", -3, "logs");
        assert_eq!(
            document,
            doc! {
                "_id": { "archive_id": "a.clps", "log_event_idx": Bson::Int64(7) },
                "orig_file_path": "",
                "message": "{\"a\":1}\n",
                "timestamp": Bson::Int64(-3),
                "dataset": "logs",
            }
        );
        let keys: Vec<&str> = document.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["_id", "orig_file_path", "message", "timestamp", "dataset"]
        );
    }

    #[test]
    fn invalid_utf8_messages_are_replaced() {
        let document = search_result_document("a", 0, b"{\"a\":\"\xc3\"}", 0, "");
        assert_eq!(
            document.get_str("message").ok(),
            Some("{\"a\":\"\u{fffd}\"}\n")
        );
        assert_eq!(document.get_i64("timestamp").ok(), Some(0));
    }

    #[test]
    fn the_default_batch_size_is_the_cpp_default() {
        assert_eq!(ResultsCacheConfig::default().batch_size, 1000);
    }
}
