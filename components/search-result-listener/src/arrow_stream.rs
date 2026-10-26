//! The Arrow IPC stream that a protocol-v3 connection carries between its handshake and its
//! trailer, and the record batches of results it delivers.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;

use arrow_array::Array;
use arrow_array::ArrayRef;
use arrow_array::Int64Array;
use arrow_array::RecordBatch;
use arrow_array::StringArray;
use arrow_buffer::Buffer;
use arrow_ipc::MessageHeader;
use arrow_ipc::reader::RecordBatchDecoder;
use arrow_schema::ArrowError;
use arrow_schema::DataType;
use arrow_schema::Field;
use arrow_schema::Schema;
use arrow_schema::SchemaRef;
use bytes::Buf;
use bytes::Bytes;
use bytes::BytesMut;
use clp_rust_utils::types::ArchiveId;

use crate::error::ProtocolError;

/// The name of the column that holds each result's archive ID.
pub const ARCHIVE_ID_COLUMN: &str = "archive_id";

/// The name of the column that holds each result's timestamp, in epoch milliseconds.
pub const TIMESTAMP_COLUMN: &str = "timestamp";

/// The name of the column that holds each result's log message, without a trailing newline.
pub const MESSAGE_COLUMN: &str = "message";

/// The schema of every result batch. The archive ID column is dictionary-encoded, since the
/// results of a connection share one archive; its dictionary is the stream's only one, so its ID is
/// 0.
pub static RESULT_SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new_dictionary(ARCHIVE_ID_COLUMN, DataType::Int32, DataType::Utf8, false),
        Field::new(TIMESTAMP_COLUMN, DataType::Int64, false),
        Field::new(MESSAGE_COLUMN, DataType::Utf8, false),
    ]))
});

/// The results of one record batch that a protocol-v3 connection streamed, validated and decoded
/// without copying, together with the IPC messages that carry them.
#[derive(Clone, Debug)]
pub struct ArrowResultBatch {
    dictionary_message: Bytes,
    record_batch_message: Bytes,
    record_batch: RecordBatch,
}

impl ArrowResultBatch {
    /// # Returns
    ///
    /// The number of results in the batch.
    #[must_use]
    pub fn num_rows(&self) -> usize {
        self.record_batch.num_rows()
    }

    /// # Returns
    ///
    /// The framed IPC messages that carry the batch in a stream of [`RESULT_SCHEMA`]: the
    /// connection's archive ID dictionary batch, then the record batch, both exactly as the
    /// connection streamed them.
    #[must_use]
    pub const fn ipc_messages(&self) -> [&Bytes; 2] {
        [&self.dictionary_message, &self.record_batch_message]
    }

    /// # Returns
    ///
    /// The decoded batch, of [`RESULT_SCHEMA`].
    #[must_use]
    pub const fn record_batch(&self) -> &RecordBatch {
        &self.record_batch
    }

    /// # Returns
    ///
    /// The timestamp column of the batch.
    ///
    /// # Panics
    ///
    /// Panics if the column isn't `Int64`, which decoding with [`RESULT_SCHEMA`] rules out.
    #[must_use]
    pub fn timestamps(&self) -> &Int64Array {
        self.record_batch
            .column(TIMESTAMP_COLUMN_INDEX)
            .as_any()
            .downcast_ref::<Int64Array>()
            .expect("the timestamp column of a decoded batch should be `Int64`")
    }

    /// # Returns
    ///
    /// The message column of the batch.
    ///
    /// # Panics
    ///
    /// Panics if the column isn't `Utf8`, which decoding with [`RESULT_SCHEMA`] rules out.
    #[must_use]
    pub fn messages(&self) -> &StringArray {
        self.record_batch
            .column(MESSAGE_COLUMN_INDEX)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("the message column of a decoded batch should be `Utf8`")
    }
}

/// Reads the Arrow IPC stream of a protocol-v3 connection, checking it against the connection's
/// archive, and fully validating every record batch.
#[derive(Debug)]
pub struct ArrowStreamReader {
    archive_id: String,
    state: State,
}

impl ArrowStreamReader {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A reader of the stream of a connection that streams the results of the archive
    /// `archive_id`.
    #[must_use]
    pub fn new(archive_id: ArchiveId) -> Self {
        Self {
            archive_id: archive_id.to_string(),
            state: State::ExpectSchema,
        }
    }

    /// # Returns
    ///
    /// Whether the reader has read the end-of-stream marker.
    #[must_use]
    pub const fn has_ended(&self) -> bool {
        matches!(self.state, State::Ended)
    }

    /// Reads the next frame of the stream from the front of `buffer`, consuming its bytes only if
    /// the frame is complete.
    ///
    /// # Returns
    ///
    /// On success:
    ///
    /// * [`ArrowRead::Batch`] with the next record batch.
    /// * [`ArrowRead::Continue`] if the reader consumed a frame that carries no results.
    /// * [`ArrowRead::Incomplete`] with the number of bytes `buffer` must hold for the next frame
    ///   to be complete.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ProtocolError::MalformedArrowStream`] if:
    ///   * The stream's schema isn't [`RESULT_SCHEMA`].
    ///   * The stream doesn't send exactly one non-delta archive ID dictionary, holding only the
    ///     connection's archive ID, right after its schema.
    ///   * A record batch fails validation against the schema and the dictionary.
    ///   * A message of an unexpected type arrives.
    ///   * The stream ends before its dictionary.
    /// * Forwards [`split_message`]'s return values on failure.
    ///
    /// # Panics
    ///
    /// Panics if called after the stream has ended.
    pub fn read(&mut self, buffer: &mut BytesMut) -> Result<ArrowRead, ProtocolError> {
        assert!(!self.has_ended(), "the Arrow stream has already ended");
        let message = match split_message(buffer)? {
            IpcFrame::Message(message) => message,
            IpcFrame::Incomplete(num_bytes_needed) => {
                return Ok(ArrowRead::Incomplete(num_bytes_needed));
            }
            IpcFrame::EndOfStream => {
                if !matches!(self.state, State::Batches { .. }) {
                    return Err(malformed("the stream ended before its dictionary"));
                }
                self.state = State::Ended;
                return Ok(ArrowRead::Continue);
            }
        };
        let header = message.header();
        match (&self.state, header.header_type()) {
            (State::ExpectSchema, MessageHeader::Schema) => {
                let schema = header
                    .header_as_schema()
                    .ok_or_else(|| malformed("the schema message holds no schema"))?;
                let schema =
                    arrow_ipc::convert::try_fb_to_schema(schema).map_err(|e| arrow_error(&e))?;
                if schema.fields() != RESULT_SCHEMA.fields() {
                    return Err(malformed(&format!("unexpected schema: {schema:?}")));
                }
                self.state = State::ExpectDictionary;
                Ok(ArrowRead::Continue)
            }
            (State::ExpectDictionary, MessageHeader::DictionaryBatch) => {
                let dictionaries = self.read_archive_id_dictionary(&message)?;
                self.state = State::Batches {
                    dictionary_message: message.bytes,
                    dictionaries,
                };
                Ok(ArrowRead::Continue)
            }
            (
                State::Batches {
                    dictionary_message,
                    dictionaries,
                },
                MessageHeader::RecordBatch,
            ) => {
                let record_batch = decode_record_batch(&message, dictionaries)?;
                Ok(ArrowRead::Batch(ArrowResultBatch {
                    dictionary_message: dictionary_message.clone(),
                    record_batch_message: message.bytes,
                    record_batch,
                }))
            }
            (_, header_type) => Err(malformed(&format!(
                "unexpected IPC message of type {header_type:?}"
            ))),
        }
    }

    /// Decodes the archive ID dictionary batch, and checks that it holds only the connection's
    /// archive ID.
    ///
    /// # Returns
    ///
    /// The dictionaries of the stream on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ProtocolError::MalformedArrowStream`] if:
    ///   * The message holds no dictionary batch.
    ///   * The dictionary batch is a delta or isn't the archive ID column's.
    ///   * The dictionary doesn't hold exactly the connection's archive ID.
    /// * Forwards [`arrow_ipc::reader::read_dictionary`]'s return values on failure.
    fn read_archive_id_dictionary(
        &self,
        message: &IpcMessage,
    ) -> Result<HashMap<i64, ArrayRef>, ProtocolError> {
        let header = message.header();
        let dictionary_batch = header
            .header_as_dictionary_batch()
            .ok_or_else(|| malformed("the dictionary message holds no dictionary batch"))?;
        if ARCHIVE_ID_DICTIONARY_ID != dictionary_batch.id() || dictionary_batch.isDelta() {
            return Err(malformed(
                "the dictionary batch isn't a replacement of the archive ID dictionary",
            ));
        }
        let mut dictionaries = HashMap::new();
        arrow_ipc::reader::read_dictionary(
            &Buffer::from(message.body()),
            dictionary_batch,
            &RESULT_SCHEMA,
            &mut dictionaries,
            &header.version(),
        )
        .map_err(|e| arrow_error(&e))?;
        let values = dictionaries
            .get(&ARCHIVE_ID_DICTIONARY_ID)
            .and_then(|values| values.as_any().downcast_ref::<StringArray>())
            .ok_or_else(|| malformed("the archive ID dictionary isn't a string array"))?;
        if 1 != values.len() || values.is_null(0) || values.value(0) != self.archive_id {
            return Err(malformed(
                "the archive ID dictionary doesn't hold exactly the connection's archive ID",
            ));
        }
        Ok(dictionaries)
    }
}

/// The outcome of [`ArrowStreamReader::read`].
#[derive(Debug)]
pub enum ArrowRead {
    /// A record batch of results.
    Batch(ArrowResultBatch),

    /// A frame that carries no results was consumed.
    Continue,

    /// The buffer ends before the next frame does; the frame needs this many bytes.
    Incomplete(usize),
}

const ARCHIVE_ID_DICTIONARY_ID: i64 = 0;
const TIMESTAMP_COLUMN_INDEX: usize = 1;
const MESSAGE_COLUMN_INDEX: usize = 2;
const CONTINUATION_MARKER: [u8; 4] = [0xff; 4];
const IPC_ALIGNMENT: usize = 8;

/// The bytes before an IPC message's metadata: the continuation marker and the metadata length.
const IPC_PREFIX_LEN: usize = 8;

const MAX_IPC_METADATA_LEN: usize = 1024 * 1024;
const MAX_IPC_MESSAGE_LEN: usize = 64 * 1024 * 1024;

/// Messages shorter than this are copied out of the connection's buffer instead of sharing its
/// allocation, which spans at least one socket read of 64 KiB: a short message forwarded to a
/// session's consumer would otherwise keep alive an allocation many times its size, beyond what the
/// session's byte budget counts.
const MIN_SHARED_MESSAGE_LEN: usize = 64 * 1024;

/// The state of an [`ArrowStreamReader`].
#[derive(Debug)]
enum State {
    ExpectSchema,
    ExpectDictionary,
    Batches {
        dictionary_message: Bytes,
        dictionaries: HashMap<i64, ArrayRef>,
    },
    Ended,
}

/// One encapsulated IPC message, framed as in the IPC streaming format: the continuation marker,
/// the metadata length, the `Message` flatbuffer, then the body.
#[derive(Debug)]
struct IpcMessage {
    bytes: Bytes,
    metadata_len: usize,
}

impl IpcMessage {
    /// # Returns
    ///
    /// The message's `Message` flatbuffer.
    ///
    /// # Panics
    ///
    /// Panics if the flatbuffer is invalid, which [`split_message`] verified it isn't.
    fn header(&self) -> arrow_ipc::Message<'_> {
        arrow_ipc::root_as_message(&self.bytes[IPC_PREFIX_LEN..IPC_PREFIX_LEN + self.metadata_len])
            .expect("the message's flatbuffer should have been verified")
    }

    /// # Returns
    ///
    /// The message's body.
    fn body(&self) -> Bytes {
        self.bytes.slice(IPC_PREFIX_LEN + self.metadata_len..)
    }
}

/// A frame of the IPC stream split from a connection's buffer.
#[derive(Debug)]
enum IpcFrame {
    /// A complete message.
    Message(IpcMessage),

    /// The end-of-stream marker.
    EndOfStream,

    /// The buffer ends before the frame does; the frame needs this many bytes.
    Incomplete(usize),
}

/// Splits one framed IPC message from the front of `buffer`, consuming its bytes only if it is
/// complete. A message shorter than [`MIN_SHARED_MESSAGE_LEN`] is copied out of `buffer`.
///
/// # Returns
///
/// The split frame on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`ProtocolError::MalformedArrowStream`] if:
///   * The frame doesn't start with the continuation marker.
///   * The metadata length isn't a multiple of 8, or exceeds [`MAX_IPC_METADATA_LEN`].
///   * The metadata isn't a valid `Message` flatbuffer.
///   * The message's body length is negative or isn't a multiple of 8.
///   * The message's length exceeds [`MAX_IPC_MESSAGE_LEN`].
fn split_message(buffer: &mut BytesMut) -> Result<IpcFrame, ProtocolError> {
    if buffer.len() < IPC_PREFIX_LEN {
        return Ok(IpcFrame::Incomplete(IPC_PREFIX_LEN));
    }
    if buffer[..CONTINUATION_MARKER.len()] != CONTINUATION_MARKER {
        return Err(malformed(
            "an IPC message doesn't start with the continuation marker",
        ));
    }
    let metadata_len = u32::from_le_bytes(
        buffer[CONTINUATION_MARKER.len()..IPC_PREFIX_LEN]
            .try_into()
            .expect("the metadata length should be 4 bytes"),
    ) as usize;
    if 0 == metadata_len {
        buffer.advance(IPC_PREFIX_LEN);
        return Ok(IpcFrame::EndOfStream);
    }
    if !metadata_len.is_multiple_of(IPC_ALIGNMENT) {
        return Err(malformed(&format!(
            "IPC metadata of {metadata_len} bytes isn't 8-byte aligned"
        )));
    }
    if metadata_len > MAX_IPC_METADATA_LEN {
        return Err(malformed(&format!(
            "IPC metadata of {metadata_len} bytes exceeds the limit of {MAX_IPC_METADATA_LEN} \
             bytes"
        )));
    }
    let header_len = IPC_PREFIX_LEN + metadata_len;
    if buffer.len() < header_len {
        return Ok(IpcFrame::Incomplete(header_len));
    }
    let message = arrow_ipc::root_as_message(&buffer[IPC_PREFIX_LEN..header_len])
        .map_err(|e| malformed(&format!("invalid IPC metadata: {e}")))?;
    let body_len = usize::try_from(message.bodyLength())
        .map_err(|_| malformed("negative IPC message body length"))?;
    if !body_len.is_multiple_of(IPC_ALIGNMENT) {
        return Err(malformed(&format!(
            "IPC message body of {body_len} bytes isn't 8-byte aligned"
        )));
    }
    let message_len = header_len.saturating_add(body_len);
    if message_len > MAX_IPC_MESSAGE_LEN {
        return Err(malformed(&format!(
            "IPC message of {message_len} bytes exceeds the limit of {MAX_IPC_MESSAGE_LEN} bytes"
        )));
    }
    if buffer.len() < message_len {
        return Ok(IpcFrame::Incomplete(message_len));
    }
    let bytes = if message_len < MIN_SHARED_MESSAGE_LEN {
        let bytes = Bytes::copy_from_slice(&buffer[..message_len]);
        buffer.advance(message_len);
        bytes
    } else {
        buffer.split_to(message_len).freeze()
    };
    Ok(IpcFrame::Message(IpcMessage {
        bytes,
        metadata_len,
    }))
}

/// Decodes and fully validates a record batch message of [`RESULT_SCHEMA`], without copying its
/// buffers unless they are misaligned.
///
/// # Returns
///
/// The decoded batch on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`ProtocolError::MalformedArrowStream`] if:
///   * The message holds no record batch.
///   * The record batch fails decoding or validation.
fn decode_record_batch(
    message: &IpcMessage,
    dictionaries: &HashMap<i64, ArrayRef>,
) -> Result<RecordBatch, ProtocolError> {
    let header = message.header();
    let record_batch = header
        .header_as_record_batch()
        .ok_or_else(|| malformed("the record batch message holds no record batch"))?;
    let body = Buffer::from(message.body());
    let version = header.version();
    RecordBatchDecoder::try_new(
        &body,
        record_batch,
        Arc::clone(&RESULT_SCHEMA),
        dictionaries,
        &version,
    )
    .and_then(RecordBatchDecoder::read_record_batch)
    .map_err(|e| arrow_error(&e))
}

/// # Returns
///
/// A [`ProtocolError::MalformedArrowStream`] described by `description`.
fn malformed(description: &str) -> ProtocolError {
    ProtocolError::MalformedArrowStream(description.to_owned())
}

/// # Returns
///
/// The [`ProtocolError::MalformedArrowStream`] of an Arrow decoding or validation error.
fn arrow_error(error: &ArrowError) -> ProtocolError {
    ProtocolError::MalformedArrowStream(format!("invalid Arrow data: {error}"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::ArrayRef;
    use arrow_array::DictionaryArray;
    use arrow_array::Int32Array;
    use arrow_array::Int64Array;
    use arrow_array::RecordBatch;
    use arrow_array::StringArray;
    use arrow_array::types::Int32Type;
    use arrow_ipc::writer::DictionaryHandling;
    use arrow_ipc::writer::IpcWriteOptions;
    use arrow_ipc::writer::StreamWriter;
    use arrow_schema::DataType;
    use arrow_schema::Field;
    use arrow_schema::Schema;
    use arrow_schema::SchemaRef;
    use bytes::BytesMut;
    use clp_rust_utils::types::ArchiveId;

    use super::ArrowRead;
    use super::ArrowResultBatch;
    use super::ArrowStreamReader;
    use super::IpcFrame;
    use super::MIN_SHARED_MESSAGE_LEN;
    use super::RESULT_SCHEMA;
    use super::split_message;
    use crate::error::ProtocolError;

    const ARCHIVE_ID: &str = "018e90e5-8b2a-4a61-a2fc-cac799936caf";
    const OTHER_ARCHIVE_ID: &str = "4b0c2f8e-6d13-4f4e-9a57-1c2d3e4f5a6b";

    /// # Returns
    ///
    /// A reader of the stream of archive [`ARCHIVE_ID`].
    fn reader() -> ArrowStreamReader {
        ArrowStreamReader::new(ARCHIVE_ID.parse::<ArchiveId>().expect("valid archive UUID"))
    }

    /// # Returns
    ///
    /// A batch of [`RESULT_SCHEMA`] whose archive ID dictionary is `archive_ids`, with a message
    /// `"message {timestamp}"` per timestamp.
    fn batch(archive_ids: &[&str], timestamps: &[i64]) -> RecordBatch {
        let num_rows = timestamps.len();
        let keys = Int32Array::from(vec![0; num_rows]);
        let values: ArrayRef = Arc::new(StringArray::from(archive_ids.to_vec()));
        let messages: Vec<String> = timestamps
            .iter()
            .map(|timestamp| format!("message {timestamp}"))
            .collect();
        RecordBatch::try_new(
            Arc::clone(&RESULT_SCHEMA),
            vec![
                Arc::new(
                    DictionaryArray::<Int32Type>::try_new(keys, values)
                        .expect("the keys should index the dictionary"),
                ),
                Arc::new(Int64Array::from(timestamps.to_vec())),
                Arc::new(StringArray::from(messages)),
            ],
        )
        .expect("the batch should match the result schema")
    }

    /// # Returns
    ///
    /// The bytes of an IPC stream of `schema` holding `batches`, with replacement dictionaries
    /// resent whenever they change.
    fn stream(schema: &SchemaRef, batches: &[RecordBatch]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let options =
            IpcWriteOptions::default().with_dictionary_handling(DictionaryHandling::Resend);
        let mut writer = StreamWriter::try_new_with_options(&mut bytes, schema, options)
            .expect("writing to a `Vec` shouldn't fail");
        for batch in batches {
            writer
                .write(batch)
                .expect("writing to a `Vec` shouldn't fail");
        }
        writer.finish().expect("writing to a `Vec` shouldn't fail");
        drop(writer);
        bytes
    }

    /// # Returns
    ///
    /// The framed IPC messages of `bytes`, the end-of-stream marker being the last.
    fn messages(bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut buffer = BytesMut::from(bytes);
        let mut messages = Vec::new();
        while !buffer.is_empty() {
            let len = buffer.len();
            match split_message(&mut buffer).expect("the stream should be well framed") {
                IpcFrame::Message(message) => messages.push(message.bytes.to_vec()),
                IpcFrame::EndOfStream => messages.push(bytes[bytes.len() - len..][..8].to_vec()),
                IpcFrame::Incomplete(_) => panic!("the stream shouldn't be truncated"),
            }
        }
        messages
    }

    /// Reads every frame of `bytes` with `reader`, until the stream ends or a frame is incomplete.
    ///
    /// # Returns
    ///
    /// The record batches read on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`ArrowStreamReader::read`]'s return values on failure.
    fn read_all(
        reader: &mut ArrowStreamReader,
        bytes: &[u8],
    ) -> Result<Vec<ArrowResultBatch>, ProtocolError> {
        let mut buffer = BytesMut::from(bytes);
        let mut batches = Vec::new();
        while !reader.has_ended() {
            match reader.read(&mut buffer)? {
                ArrowRead::Batch(batch) => batches.push(batch),
                ArrowRead::Continue => {}
                ArrowRead::Incomplete(_) => break,
            }
        }
        Ok(batches)
    }

    /// Checks that reading `bytes` fails with a [`ProtocolError::MalformedArrowStream`] whose
    /// description contains `description`.
    fn expect_malformed(bytes: &[u8], description: &str) {
        let error = read_all(&mut reader(), bytes).expect_err("the stream should be rejected");
        let ProtocolError::MalformedArrowStream(actual) = &error else {
            panic!("expected a malformed Arrow stream, got {error:?}");
        };
        assert!(
            actual.contains(description),
            "expected {description:?}, got {actual:?}"
        );
    }

    #[test]
    fn valid_stream_yields_its_batches_with_their_exact_messages() {
        let bytes = stream(
            &RESULT_SCHEMA,
            &[batch(&[ARCHIVE_ID], &[1, 2, 3]), batch(&[ARCHIVE_ID], &[4])],
        );
        let messages = messages(&bytes);
        let mut reader = reader();
        let batches = read_all(&mut reader, &bytes).expect("the stream should be valid");

        assert!(reader.has_ended());
        assert_eq!(batches.len(), 2);
        assert_eq!(
            batches[0].timestamps().values().to_vec(),
            [1_i64, 2, 3].to_vec()
        );
        assert_eq!(batches[1].messages().value(0), "message 4");
        // The schema, the dictionary, two record batches, the end-of-stream marker.
        assert_eq!(messages.len(), 5);
        for (batch, record_batch_message) in batches.iter().zip(&messages[2..4]) {
            let [dictionary, record_batch] = batch.ipc_messages();
            assert_eq!(dictionary.as_ref(), messages[1].as_slice());
            assert_eq!(record_batch.as_ref(), record_batch_message.as_slice());
        }
    }

    #[test]
    fn only_long_messages_share_the_buffer_allocation() {
        let timestamps: Vec<i64> = (0..10_000).collect();
        let bytes = stream(
            &RESULT_SCHEMA,
            &[
                batch(&[ARCHIVE_ID], &[1]),
                batch(&[ARCHIVE_ID], &timestamps),
            ],
        );
        let mut buffer = BytesMut::from(&bytes[..]);
        let allocation = buffer.as_ptr().addr()..buffer.as_ptr().addr() + buffer.capacity();

        let mut messages = Vec::new();
        while let IpcFrame::Message(message) =
            split_message(&mut buffer).expect("the stream should be well framed")
        {
            messages.push((
                message.bytes.len() >= MIN_SHARED_MESSAGE_LEN,
                allocation.contains(&message.bytes.as_ptr().addr()),
            ));
        }

        // The schema, the dictionary, the short record batch, then the long one.
        assert_eq!(
            messages,
            [(false, false), (false, false), (false, false), (true, true)]
        );
    }

    #[test]
    fn incomplete_message_is_left_in_the_buffer_with_its_length() {
        let bytes = stream(&RESULT_SCHEMA, &[batch(&[ARCHIVE_ID], &[1])]);
        let schema_len = messages(&bytes)[0].len();
        let mut reader = reader();
        for prefix_len in [0, 4, 8, schema_len - 1] {
            let mut buffer = BytesMut::from(&bytes[..prefix_len]);
            let read = reader.read(&mut buffer).expect("a prefix should be valid");
            assert_eq!(buffer.len(), prefix_len);
            match (prefix_len, read) {
                (0..8, ArrowRead::Incomplete(8)) => {}
                (_, ArrowRead::Incomplete(num_bytes_needed)) if num_bytes_needed >= 8 => {
                    assert!(
                        num_bytes_needed <= schema_len,
                        "{prefix_len}: {num_bytes_needed}"
                    );
                }
                (_, read) => panic!("{prefix_len}: unexpected read {read:?}"),
            }
        }
        let mut buffer = BytesMut::from(&bytes[..schema_len - 1]);
        assert!(matches!(
            reader.read(&mut buffer),
            Ok(ArrowRead::Incomplete(num_bytes_needed)) if schema_len == num_bytes_needed
        ));
    }

    #[test]
    fn other_schema_is_rejected() {
        let schema = Arc::new(Schema::new(vec![
            Field::new_dictionary("archive_id", DataType::Int32, DataType::Utf8, false),
            Field::new("timestamp", DataType::Int64, false),
            Field::new("message", DataType::LargeUtf8, false),
        ]));
        expect_malformed(&stream(&schema, &[]), "unexpected schema");
    }

    #[test]
    fn second_dictionary_is_rejected() {
        let bytes = stream(
            &RESULT_SCHEMA,
            &[batch(&[ARCHIVE_ID], &[1]), batch(&[OTHER_ARCHIVE_ID], &[2])],
        );
        expect_malformed(&bytes, "unexpected IPC message of type DictionaryBatch");
    }

    #[test]
    fn dictionary_of_another_archive_or_values_is_rejected() {
        for archive_ids in [&[OTHER_ARCHIVE_ID][..], &[ARCHIVE_ID, OTHER_ARCHIVE_ID]] {
            expect_malformed(
                &stream(&RESULT_SCHEMA, &[batch(archive_ids, &[1])]),
                "doesn't hold exactly the connection's archive ID",
            );
        }
    }

    #[test]
    fn record_batch_before_the_dictionary_is_rejected() {
        let messages = messages(&stream(&RESULT_SCHEMA, &[batch(&[ARCHIVE_ID], &[1])]));
        let without_dictionary = [messages[0].as_slice(), messages[2].as_slice()].concat();
        expect_malformed(
            &without_dictionary,
            "unexpected IPC message of type RecordBatch",
        );
        let without_batches = [messages[0].as_slice(), messages[3].as_slice()].concat();
        expect_malformed(&without_batches, "ended before its dictionary");
    }

    #[test]
    fn record_batch_with_invalid_utf8_is_rejected() {
        let mut bytes = stream(&RESULT_SCHEMA, &[batch(&[ARCHIVE_ID], &[7])]);
        let position = bytes
            .windows(9)
            .position(|window| window == b"message 7")
            .expect("the stream should hold the message");
        bytes[position] = 0xff;
        expect_malformed(&bytes, "invalid Arrow data");
    }

    #[test]
    fn malformed_framing_is_rejected() {
        let bytes = stream(&RESULT_SCHEMA, &[batch(&[ARCHIVE_ID], &[1])]);
        let mut no_marker = bytes.clone();
        no_marker[0] = 0;
        expect_malformed(&no_marker, "continuation marker");

        let mut unaligned = bytes;
        unaligned[4] = 3;
        expect_malformed(&unaligned, "isn't 8-byte aligned");

        let mut garbage = vec![0xff, 0xff, 0xff, 0xff, 16, 0, 0, 0];
        garbage.extend_from_slice(&[0xab; 16]);
        expect_malformed(&garbage, "invalid IPC metadata");

        let oversized_metadata = [0xff, 0xff, 0xff, 0xff, 0xf8, 0xff, 0xff, 0x7f];
        expect_malformed(&oversized_metadata, "exceeds the limit");
    }

    #[test]
    fn oversized_or_unaligned_body_is_rejected_before_it_arrives() {
        let messages = messages(&stream(&RESULT_SCHEMA, &[batch(&[ARCHIVE_ID], &[1])]));
        let dictionary = &messages[1];
        let metadata_len = usize::try_from(u32::from_le_bytes(
            dictionary[4..8]
                .try_into()
                .expect("the metadata length should be 4 bytes"),
        ))
        .expect("the metadata length should fit in `usize`");
        let header_len = 8 + metadata_len;
        let body_len = u64::try_from(dictionary.len() - header_len)
            .expect("the body length should fit in `u64`");
        let body_len_position = dictionary[8..header_len]
            .windows(8)
            .position(|window| window == body_len.to_le_bytes())
            .expect("the metadata should hold the body length")
            + 8;

        for (patched_body_len, description) in [
            (1_u64 << 40, "exceeds the limit"),
            ((u64::MAX >> 1) & !7, "exceeds the limit"),
            (body_len + 1, "isn't 8-byte aligned"),
        ] {
            let mut header = dictionary[..header_len].to_vec();
            header[body_len_position..body_len_position + 8]
                .copy_from_slice(&patched_body_len.to_le_bytes());
            let error = split_message(&mut BytesMut::from(&header[..]))
                .expect_err("the body length should be rejected");
            let ProtocolError::MalformedArrowStream(actual) = &error else {
                panic!("expected a malformed Arrow stream, got {error:?}");
            };
            assert!(
                actual.contains(description),
                "expected {description:?}, got {actual:?}"
            );
        }
    }
}
