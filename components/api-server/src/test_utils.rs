//! Utilities for testing the Arrow IPC stream responses of streaming searches.

use std::ops::Range;
use std::sync::Arc;
use std::sync::LazyLock;

use arrow_array::Array;
use arrow_array::ArrayRef;
use arrow_array::DictionaryArray;
use arrow_array::Int32Array;
use arrow_array::Int64Array;
use arrow_array::RecordBatch;
use arrow_array::StringArray;
use arrow_array::types::Int32Type;
use arrow_ipc::CompressionType;
use arrow_ipc::MessageHeader;
use arrow_ipc::reader::StreamReader;
use search_result_listener::RESULT_SCHEMA;

/// The archive whose results the tests stream.
pub const ARCHIVE_ID: &str = "018e90e5-8b2a-4a61-a2fc-cac799936caf";

/// One result decoded from an Arrow IPC stream: `(archive_id, timestamp, message)`.
pub type ArrowRow = (String, i64, String);

/// # Returns
///
/// The timestamp of the result at `result_index`.
pub fn timestamp(result_index: u64) -> i64 {
    1_700_000_000_000 + i64::try_from(result_index).expect("test result indices fit in `i64`")
}

/// # Returns
///
/// The message of the result at `result_index`, without a trailing newline.
pub fn message(result_index: u64) -> String {
    format!("{{\"result\":{result_index}}}")
}

/// # Returns
///
/// The rows of the results at `result_indices`, as an Arrow IPC stream holds them.
pub fn expected_rows(result_indices: Range<u64>) -> Vec<ArrowRow> {
    result_indices
        .map(|result_index| {
            (
                ARCHIVE_ID.to_owned(),
                timestamp(result_index),
                message(result_index),
            )
        })
        .collect()
}

/// Builds the record batch of the results at `result_indices`, as a protocol-v3 search task
/// streams it.
///
/// Every batch shares one archive ID dictionary, so that an IPC writer writes it only once.
///
/// # Returns
///
/// The record batch.
///
/// # Panics
///
/// Panics if the batch doesn't match the result schema.
pub fn result_batch(result_indices: Range<u64>) -> RecordBatch {
    static ARCHIVE_IDS: LazyLock<ArrayRef> =
        LazyLock::new(|| Arc::new(StringArray::from(vec![ARCHIVE_ID])));

    let num_rows = usize::try_from(result_indices.end - result_indices.start)
        .expect("the number of test results should fit in `usize`");
    let archive_ids = DictionaryArray::<Int32Type>::try_new(
        Int32Array::from(vec![0; num_rows]),
        Arc::clone(&ARCHIVE_IDS),
    )
    .expect("the keys should index the dictionary");
    RecordBatch::try_new(
        Arc::clone(&RESULT_SCHEMA),
        vec![
            Arc::new(archive_ids),
            Arc::new(Int64Array::from_iter_values(
                result_indices.clone().map(timestamp),
            )),
            Arc::new(StringArray::from_iter_values(result_indices.map(message))),
        ],
    )
    .expect("the batch should match the result schema")
}

/// Decodes the result stream at the start of `stream` with arrow-rs's stream reader, and advances
/// `stream` past it.
///
/// # Returns
///
/// The results of the stream.
///
/// # Panics
///
/// Panics if `stream` doesn't start with a result stream.
pub fn decode_result_stream(stream: &mut &[u8]) -> Vec<ArrowRow> {
    let reader =
        StreamReader::try_new(stream, None).expect("the stream should start with a schema");
    assert_eq!(reader.schema(), *RESULT_SCHEMA);
    let mut rows = Vec::new();
    for batch in reader {
        let batch = batch.expect("every result batch should decode");
        let archive_ids = batch
            .column(0)
            .as_any()
            .downcast_ref::<DictionaryArray<Int32Type>>()
            .expect("archive IDs should be a dictionary");
        let archive_id_values = archive_ids
            .values()
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("archive ID values should be strings");
        let timestamps = batch
            .column(1)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("timestamps should be `Int64`");
        let messages = batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("messages should be strings");
        for row in 0..batch.num_rows() {
            let key = usize::try_from(archive_ids.keys().value(row))
                .expect("keys should be non-negative");
            rows.push((
                archive_id_values.value(key).to_owned(),
                timestamps.value(row),
                messages.value(row).to_owned(),
            ));
        }
    }
    rows
}

/// Walks the framed IPC messages at the start of `stream`, up to its end-of-stream marker.
///
/// # Returns
///
/// The header type of each message, and the compression codec of its body, if any.
///
/// # Panics
///
/// Panics if `stream` doesn't start with framed IPC messages that end with an end-of-stream marker.
pub fn ipc_message_codecs(mut stream: &[u8]) -> Vec<(MessageHeader, Option<CompressionType>)> {
    let mut codecs = Vec::new();
    loop {
        let metadata_len = usize::try_from(u32::from_le_bytes(
            stream[4..8]
                .try_into()
                .expect("the metadata length should be 4 bytes"),
        ))
        .expect("the metadata length should fit in `usize`");
        if 0 == metadata_len {
            return codecs;
        }
        let message = arrow_ipc::root_as_message(&stream[8..8 + metadata_len])
            .expect("the metadata should be a message");
        let compression = match message.header_type() {
            MessageHeader::RecordBatch => message.header_as_record_batch(),
            MessageHeader::DictionaryBatch => message
                .header_as_dictionary_batch()
                .and_then(|dictionary_batch| dictionary_batch.data()),
            _ => None,
        }
        .and_then(|record_batch| record_batch.compression())
        .map(|compression| compression.codec());
        codecs.push((message.header_type(), compression));
        let body_len =
            usize::try_from(message.bodyLength()).expect("the body length should be non-negative");
        stream = &stream[8 + metadata_len + body_len..];
    }
}
