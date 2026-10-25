//! The writer of the Arrow IPC stream of one archive's search results.

use std::num::NonZeroUsize;
use std::ops::Range;
use std::sync::Arc;

use arrow_array::ArrayRef;
use arrow_array::DictionaryArray;
use arrow_array::Int32Array;
use arrow_array::Int64Array;
use arrow_array::StringArray;
use arrow_array::types::Int32Type;
use arrow_buffer::Buffer;
use arrow_buffer::OffsetBuffer;
use arrow_buffer::ScalarBuffer;
use arrow_ipc::MetadataVersion;
use arrow_ipc::writer::IpcWriteOptions;
use arrow_ipc::writer::StreamEncoder;
use arrow_schema::ArrowError;
use clp_s_reader::JsonRecordBuffer;
use clp_s_wire::epoch_ns_to_wire_ms;

use crate::RESULT_SCHEMA;

/// The limits of the record batches that a [`ResultStreamWriter`] writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResultBatchLimits {
    /// The most rows of one record batch.
    pub max_num_rows: NonZeroUsize,

    /// The most bytes of column data of one record batch, unless its only row holds more.
    pub max_size: usize,
}

impl Default for ResultBatchLimits {
    fn default() -> Self {
        Self {
            max_num_rows: NonZeroUsize::new(4096).expect("4096 is nonzero"),
            max_size: 1024 * 1024,
        }
    }
}

impl ResultBatchLimits {
    /// # Returns
    ///
    /// A tuple containing:
    ///
    /// * The end of the record batch that starts at record `begin` of `records`, the batch holding
    ///   as many records as the limits allow and at least one.
    /// * The number of bytes of the batch's messages.
    ///
    /// # Panics
    ///
    /// Panics if `begin` isn't below the number of records.
    fn batch_end(&self, records: &JsonRecordBuffer, begin: usize) -> (usize, usize) {
        assert!(
            begin < records.len(),
            "record {begin} is past the last record"
        );
        let max_end = records
            .len()
            .min(begin.saturating_add(self.max_num_rows.get()));
        let mut end = begin;
        let mut size = 0;
        let mut num_message_bytes = 0;
        while end < max_end {
            let message_len = record_text(records, end).len();
            let row_size = message_len + ROW_OVERHEAD;
            if end > begin && size + row_size > self.max_size {
                break;
            }
            size += row_size;
            num_message_bytes += message_len;
            end += 1;
        }
        (end, num_message_bytes)
    }
}

/// Writes the Arrow IPC stream of the results of one archive, of [`RESULT_SCHEMA`].
///
/// The stream holds the schema, one dictionary batch holding the archive ID, then record batches
/// whose archive ID keys are all 0.
pub struct ResultStreamWriter {
    encoder: StreamEncoder,
    limits: ResultBatchLimits,

    /// The archive ID dictionary, which every batch shares so that it's written only once.
    archive_ids: ArrayRef,

    /// Zero keys that the archive ID columns of the batches slice.
    zero_keys: Buffer,
}

impl ResultStreamWriter {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A writer of a stream of the results of the archive `archive_id`, whose record batches
    /// respect `limits`.
    ///
    /// # Panics
    ///
    /// Panics if the stream encoder rejects [`RESULT_SCHEMA`] or the IPC write options, which it
    /// never does.
    #[must_use]
    pub fn new(archive_id: &str, limits: ResultBatchLimits) -> Self {
        let options = IpcWriteOptions::try_new(IPC_ALIGNMENT, false, MetadataVersion::V5)
            .expect("the IPC write options should be valid");
        Self {
            encoder: StreamEncoder::try_new_with_options(&RESULT_SCHEMA, options)
                .expect("the result schema should be encodable"),
            limits,
            archive_ids: Arc::new(StringArray::from(vec![archive_id])),
            zero_keys: Buffer::from_vec(Vec::<i32>::new()),
        }
    }

    /// Encodes `records` as the stream's next record batches, as few as the writer's limits allow,
    /// and appends their IPC messages to `output`. The first call also appends the schema message
    /// and the archive ID dictionary batch.
    ///
    /// Each message is the JSON text of its record without the trailing newline, with invalid UTF-8
    /// replaced with U+FFFD. Each timestamp is converted to epoch milliseconds,
    /// or 0 for a record without timestamp. The appended buffers share the batches' column data
    /// rather than copying it.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ArrowError::InvalidArgumentError`] if `records` and `timestamps_ns` have different
    ///   lengths.
    /// * Forwards [`Self::record_batch`]'s return values on failure.
    /// * Forwards [`StreamEncoder::encode`]'s return values on failure.
    pub fn write(
        &mut self,
        records: &JsonRecordBuffer,
        timestamps_ns: &[Option<i64>],
        output: &mut Vec<Buffer>,
    ) -> Result<(), ArrowError> {
        if records.len() != timestamps_ns.len() {
            return Err(ArrowError::InvalidArgumentError(format!(
                "{} records have {} timestamps",
                records.len(),
                timestamps_ns.len()
            )));
        }
        let mut begin = 0;
        while begin < records.len() {
            let (end, num_message_bytes) = self.limits.batch_end(records, begin);
            let batch = self.record_batch(
                records,
                &timestamps_ns[begin..end],
                begin..end,
                num_message_bytes,
            )?;
            output.extend(self.encoder.encode(&batch)?);
            begin = end;
        }
        Ok(())
    }

    /// Ends the stream.
    ///
    /// # Returns
    ///
    /// The end-of-stream marker, preceded by the schema message if nothing was written, on
    /// success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`StreamEncoder::finish`]'s return values on failure.
    pub fn finish(self) -> Result<Vec<Buffer>, ArrowError> {
        self.encoder.finish()
    }

    /// Builds the record batch of the records at `range` of `records`, whose timestamps are
    /// `timestamps_ns` and whose messages hold `num_message_bytes` bytes.
    ///
    /// # Returns
    ///
    /// The record batch on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::archive_id_column`]'s return values on failure.
    /// * Forwards [`message_array`]'s return values on failure.
    /// * Forwards [`arrow_array::RecordBatch::try_new`]'s return values on failure.
    fn record_batch(
        &mut self,
        records: &JsonRecordBuffer,
        timestamps_ns: &[Option<i64>],
        range: Range<usize>,
        num_message_bytes: usize,
    ) -> Result<arrow_array::RecordBatch, ArrowError> {
        let timestamps: ArrayRef = Arc::new(Int64Array::from_iter_values(
            timestamps_ns
                .iter()
                .map(|timestamp_ns| timestamp_ns.map_or(0, epoch_ns_to_wire_ms)),
        ));
        let archive_ids = self.archive_id_column(range.len())?;
        let messages: ArrayRef = Arc::new(message_array(records, range, num_message_bytes)?);
        arrow_array::RecordBatch::try_new(
            Arc::clone(&RESULT_SCHEMA),
            vec![archive_ids, timestamps, messages],
        )
    }

    /// # Returns
    ///
    /// An archive ID column of `num_rows` rows, all keyed to the writer's archive ID dictionary,
    /// on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`DictionaryArray::try_new`]'s return values on failure.
    fn archive_id_column(&mut self, num_rows: usize) -> Result<ArrayRef, ArrowError> {
        if self.zero_keys.len() < num_rows * size_of::<i32>() {
            self.zero_keys = Buffer::from_vec(vec![0_i32; num_rows.next_power_of_two()]);
        }
        let keys = Int32Array::new(ScalarBuffer::new(self.zero_keys.clone(), 0, num_rows), None);
        Ok(Arc::new(DictionaryArray::<Int32Type>::try_new(
            keys,
            Arc::clone(&self.archive_ids),
        )?))
    }
}

/// The alignment of the buffers of the written IPC messages: the 8 bytes that the IPC format
/// requires, rather than arrow-rs's default of 64, which keeps small batches compact.
const IPC_ALIGNMENT: usize = 8;

/// The bytes of column data that each row adds to a record batch besides its message: an archive
/// ID key, a timestamp and a message offset.
const ROW_OVERHEAD: usize = size_of::<i32>() + size_of::<i64>() + size_of::<i32>();

/// # Returns
///
/// The JSON text of record `index` of `records`, without its trailing newline.
///
/// # Panics
///
/// Panics if `index` isn't below the number of records.
fn record_text(records: &JsonRecordBuffer, index: usize) -> &[u8] {
    records
        .record(index)
        .expect("every record below the number of records should have text")
}

/// Builds the message column of the records at `range` of `records`, whose texts hold
/// `num_message_bytes` bytes, replacing invalid UTF-8 with U+FFFD.
///
/// # Returns
///
/// The message column on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`ArrowError::OffsetOverflowError`] if the messages exceed what `i32` offsets address.
/// * Forwards [`StringArray::try_new`]'s return values on failure.
fn message_array(
    records: &JsonRecordBuffer,
    range: Range<usize>,
    num_message_bytes: usize,
) -> Result<StringArray, ArrowError> {
    let build = |is_lossy: bool| -> Result<StringArray, ArrowError> {
        let mut values = Vec::with_capacity(num_message_bytes);
        let mut offsets = Vec::with_capacity(range.len() + 1);
        offsets.push(0_i32);
        for index in range.clone() {
            let text = record_text(records, index);
            if is_lossy {
                values.extend_from_slice(String::from_utf8_lossy(text).as_bytes());
            } else {
                values.extend_from_slice(text);
            }
            offsets.push(
                i32::try_from(values.len())
                    .map_err(|_| ArrowError::OffsetOverflowError(values.len()))?,
            );
        }
        StringArray::try_new(
            OffsetBuffer::new(ScalarBuffer::from(offsets)),
            Buffer::from_vec(values),
            None,
        )
    };
    build(false).or_else(|_| build(true))
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use arrow_array::Array;
    use arrow_array::DictionaryArray;
    use arrow_array::Int64Array;
    use arrow_array::RecordBatch;
    use arrow_array::StringArray;
    use arrow_array::types::Int32Type;
    use arrow_buffer::Buffer;
    use arrow_ipc::MessageHeader;
    use clp_s_reader::JsonRecordBuffer;

    use super::ResultBatchLimits;
    use super::ResultStreamWriter;
    use crate::RESULT_SCHEMA;

    const ARCHIVE_ID: &str = "018e90e5-8b2a-4a61-a2fc-cac799936caf";

    /// # Returns
    ///
    /// A buffer holding `texts` as rendered records.
    fn records(texts: &[&[u8]]) -> JsonRecordBuffer {
        let mut records = JsonRecordBuffer::new();
        for text in texts {
            records
                .push_rendered(text)
                .expect("a small record should fit in the buffer");
        }
        records
    }

    /// # Returns
    ///
    /// The concatenation of `buffers`.
    fn concat(buffers: &[Buffer]) -> Vec<u8> {
        buffers.iter().flat_map(|buffer| buffer.to_vec()).collect()
    }

    /// # Returns
    ///
    /// The header type of each IPC message of `stream`, up to and excluding the end-of-stream
    /// marker, and the bytes that follow the marker.
    ///
    /// # Panics
    ///
    /// Panics if `stream` isn't a sequence of framed IPC messages ending with the end-of-stream
    /// marker.
    fn message_types(stream: &[u8]) -> (Vec<MessageHeader>, &[u8]) {
        let mut types = Vec::new();
        let mut remaining = stream;
        loop {
            assert_eq!(remaining[..4], [0xff; 4], "the continuation marker");
            let metadata_len = u32::from_le_bytes(
                remaining[4..8]
                    .try_into()
                    .expect("the metadata length should be 4 bytes"),
            ) as usize;
            if 0 == metadata_len {
                return (types, &remaining[8..]);
            }
            let message = arrow_ipc::root_as_message(&remaining[8..8 + metadata_len])
                .expect("the metadata should be a message");
            types.push(message.header_type());
            let body_len =
                usize::try_from(message.bodyLength()).expect("the body length should be positive");
            remaining = &remaining[8 + metadata_len + body_len..];
        }
    }

    /// # Returns
    ///
    /// The record batches of `stream`, decoded with arrow-rs's stream reader.
    ///
    /// # Panics
    ///
    /// Panics if `stream` isn't a valid Arrow IPC stream.
    fn decode(stream: &[u8]) -> Vec<RecordBatch> {
        arrow_ipc::reader::StreamReader::try_new(stream, None)
            .expect("the stream should start with a schema")
            .collect::<Result<_, _>>()
            .expect("every batch should decode")
    }

    /// # Returns
    ///
    /// The `(archive_id, timestamp, message)` rows of `batches`.
    fn rows(batches: &[RecordBatch]) -> Vec<(String, i64, String)> {
        let mut rows = Vec::new();
        for batch in batches {
            assert_eq!(batch.schema(), *RESULT_SCHEMA);
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

    #[test]
    fn stream_has_one_schema_and_one_dictionary_then_record_batches() {
        let mut writer = ResultStreamWriter::new(ARCHIVE_ID, ResultBatchLimits::default());
        let mut buffers = Vec::new();
        writer
            .write(&records(&[b"{\"a\":1}"]), &[Some(1_000_000)], &mut buffers)
            .expect("the first batch should encode");
        writer
            .write(
                &records(&[b"{\"b\":2}", b"{}"]),
                &[None, Some(-2_500_000)],
                &mut buffers,
            )
            .expect("the second batch should encode");
        buffers.extend(writer.finish().expect("the stream should end"));
        let stream = concat(&buffers);

        let (types, after_end) = message_types(&stream);
        assert_eq!(
            types,
            [
                MessageHeader::Schema,
                MessageHeader::DictionaryBatch,
                MessageHeader::RecordBatch,
                MessageHeader::RecordBatch,
            ]
        );
        assert_eq!(after_end, &[] as &[u8]);
        assert_eq!(
            rows(&decode(&stream)),
            [
                (ARCHIVE_ID.to_owned(), 1, "{\"a\":1}".to_owned()),
                (ARCHIVE_ID.to_owned(), 0, "{\"b\":2}".to_owned()),
                (ARCHIVE_ID.to_owned(), -3, "{}".to_owned()),
            ]
        );
    }

    #[test]
    fn batches_respect_the_row_and_size_limits() {
        let limits = ResultBatchLimits {
            max_num_rows: NonZeroUsize::new(3).expect("3 is nonzero"),
            max_size: 100,
        };
        let large = vec![b'x'; 150];
        let texts: Vec<&[u8]> = vec![
            b"0",
            b"1",
            b"2",
            b"3",
            &large,
            b"5",
            &[b'y'; 60],
            &[b'z'; 60],
        ];
        let mut writer = ResultStreamWriter::new(ARCHIVE_ID, limits);
        let mut buffers = Vec::new();
        writer
            .write(&records(&texts), &[None; 8], &mut buffers)
            .expect("the batches should encode");
        buffers.extend(writer.finish().expect("the stream should end"));
        let batches = decode(&concat(&buffers));

        assert_eq!(
            batches
                .iter()
                .map(RecordBatch::num_rows)
                .collect::<Vec<_>>(),
            [3, 1, 1, 2, 1],
            "3 rows, then the small row that the oversized one doesn't fit after, the oversized \
             row alone, then rows up to 100 bytes"
        );
        assert_eq!(
            rows(&batches)
                .into_iter()
                .map(|(_, _, message)| message.into_bytes())
                .collect::<Vec<_>>(),
            texts.iter().map(|text| text.to_vec()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn invalid_utf8_is_replaced_with_the_replacement_character() {
        let mut writer = ResultStreamWriter::new(ARCHIVE_ID, ResultBatchLimits::default());
        let mut buffers = Vec::new();
        writer
            .write(
                &records(&[b"{\"a\":\"ok\"}", b"{\"a\":\"\xff\xfe\"}"]),
                &[None, None],
                &mut buffers,
            )
            .expect("the batch should encode");
        buffers.extend(writer.finish().expect("the stream should end"));

        assert_eq!(
            rows(&decode(&concat(&buffers)))
                .into_iter()
                .map(|(_, _, message)| message)
                .collect::<Vec<_>>(),
            ["{\"a\":\"ok\"}", "{\"a\":\"\u{fffd}\u{fffd}\"}"]
        );
    }

    #[test]
    fn mismatched_timestamps_are_rejected() {
        let mut writer = ResultStreamWriter::new(ARCHIVE_ID, ResultBatchLimits::default());
        let mut buffers = Vec::new();
        writer
            .write(&records(&[b"{}"]), &[None, None], &mut buffers)
            .expect_err("two timestamps for one record should be rejected");
        assert_eq!(buffers, []);
    }

    #[test]
    fn empty_writes_append_nothing() {
        let mut writer = ResultStreamWriter::new(ARCHIVE_ID, ResultBatchLimits::default());
        let mut buffers = Vec::new();
        writer
            .write(&records(&[]), &[], &mut buffers)
            .expect("an empty write should succeed");
        assert_eq!(buffers, []);
    }
}
