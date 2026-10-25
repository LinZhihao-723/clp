//! Arrow IPC stream encoding of streaming search results, and the response that streams them.
//!
//! A response streams the schema of the results, then each batch as a replacement archive ID
//! dictionary and a record batch, then the end-of-stream marker, followed by a second IPC stream
//! of one row that summarizes the search.

use std::sync::Arc;
use std::sync::LazyLock;

use arrow_array::Array;
use arrow_array::ArrayRef;
use arrow_array::DictionaryArray;
use arrow_array::Int32Array;
use arrow_array::Int64Array;
use arrow_array::RecordBatch;
use arrow_array::StringArray;
use arrow_array::UInt64Array;
use arrow_array::types::Int32Type;
use arrow_buffer::Buffer;
use arrow_buffer::OffsetBuffer;
use arrow_buffer::ScalarBuffer;
use arrow_ipc::MetadataVersion;
use arrow_ipc::writer::DictionaryTracker;
use arrow_ipc::writer::EncodedData;
use arrow_ipc::writer::IpcDataGenerator;
use arrow_ipc::writer::IpcWriteContext;
use arrow_ipc::writer::IpcWriteOptions;
use arrow_ipc::writer::StreamWriter;
use arrow_schema::ArrowError;
use arrow_schema::DataType;
use arrow_schema::Field;
use arrow_schema::Schema;
use axum::BoxError;
use axum::body::Body;
use axum::body::Bytes;
use axum::http::HeaderValue;
use axum::http::header::CACHE_CONTROL;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::response::Response;
use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::types::ArchiveId;
use futures::Stream;
use search_result_listener::ArrowResultBatch;
use search_result_listener::RESULT_SCHEMA;
use search_result_listener::ResultEncoder;
use search_result_listener::ResultEncoding;
use search_result_listener::SessionOutcome;

/// The media type of an Arrow IPC stream.
pub const ARROW_STREAM_CONTENT_TYPE: &str = "application/vnd.apache.arrow.stream";

/// The header of an Arrow IPC stream response that carries the search's query job ID.
pub const QUERY_JOB_ID_HEADER: &str = "x-clp-query-job-id";

/// The end-of-stream marker of an IPC stream.
pub const END_OF_STREAM: &[u8] = &[0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0];

/// The [`ResultEncoding`] of the Arrow IPC stream responses of `POST /query/stream`.
///
/// Each batch holds, for every record batch of results, the IPC messages of the connection's
/// archive ID dictionary and of the record batch, to be streamed after [`schema_message`].
///
/// Record batches that protocol-v3 connections streamed are forwarded byte for byte, unless
/// another attempt of their task already emitted some of their results, in which case only the
/// remaining suffix is encoded again.
#[derive(Clone, Copy, Debug, Default)]
pub struct ArrowIpcEncoding;

impl ResultEncoding for ArrowIpcEncoding {
    type Batch = Vec<Bytes>;
    type Encoder = ArrowIpcEncoder;

    fn encoder(&self, archive_id: ArchiveId) -> ArrowIpcEncoder {
        ArrowIpcEncoder {
            writer: ResultIpcWriter::new(&archive_id.to_string()),
            timestamps: Vec::new(),
            offsets: vec![0],
            values: Vec::new(),
            batch: Vec::new(),
        }
    }
}

/// The [`ResultEncoder`] of [`ArrowIpcEncoding`].
pub struct ArrowIpcEncoder {
    writer: ResultIpcWriter,

    /// The timestamps of the results appended one by one since the last record batch.
    timestamps: Vec<i64>,

    /// The `Utf8` offsets of the messages of the results appended one by one.
    offsets: Vec<i32>,

    /// The bytes of the messages of the results appended one by one.
    values: Vec<u8>,

    /// The IPC messages of the pending batch.
    batch: Vec<Bytes>,
}

impl ArrowIpcEncoder {
    /// Encodes the results appended one by one, if any, as one record batch of the pending batch.
    ///
    /// # Panics
    ///
    /// Panics if encoding the record batch fails, which it never does.
    fn flush_rows(&mut self) {
        if self.timestamps.is_empty() {
            return;
        }
        let timestamps: ArrayRef = Arc::new(Int64Array::from(std::mem::take(&mut self.timestamps)));
        let offsets = std::mem::replace(&mut self.offsets, vec![0]);
        let values = std::mem::take(&mut self.values);
        let messages: ArrayRef = Arc::new(
            StringArray::try_new(
                OffsetBuffer::new(ScalarBuffer::from(offsets)),
                Buffer::from_vec(values),
                None,
            )
            .expect("the appended messages should be valid UTF-8 at monotonic offsets"),
        );
        let record_batch = self
            .writer
            .write(timestamps, messages)
            .expect("encoding a record batch of results shouldn't fail");
        self.batch.push(self.writer.dictionary_message.clone());
        self.batch.push(record_batch);
    }
}

impl ResultEncoder for ArrowIpcEncoder {
    type Batch = Vec<Bytes>;

    /// # Panics
    ///
    /// Panics if the pending messages exceed what `i32` offsets address.
    fn encode(&mut self, timestamp: i64, message: &str) {
        self.timestamps.push(timestamp);
        self.values.extend_from_slice(message.as_bytes());
        self.offsets.push(
            i32::try_from(self.values.len()).expect("the pending messages should fit in `i32`"),
        );
    }

    /// # Panics
    ///
    /// Panics if encoding the suffix of a partially duplicated batch fails, which it never does
    /// for a validated batch.
    fn encode_arrow(&mut self, batch: &ArrowResultBatch, num_duplicates: usize) {
        self.flush_rows();
        if 0 == num_duplicates {
            self.batch.extend(batch.ipc_messages().into_iter().cloned());
            return;
        }
        let suffix = batch
            .record_batch()
            .slice(num_duplicates, batch.num_rows() - num_duplicates);
        let record_batch = self
            .writer
            .write(Arc::clone(suffix.column(1)), Arc::clone(suffix.column(2)))
            .expect("encoding the suffix of a validated batch shouldn't fail");
        self.batch.push(self.writer.dictionary_message.clone());
        self.batch.push(record_batch);
    }

    fn take_batch(&mut self) -> Vec<Bytes> {
        self.flush_rows();
        std::mem::take(&mut self.batch)
    }
}

/// An Arrow IPC stream response of a streaming search.
///
/// # Type Parameters
///
/// * `BodyStreamType` - The stream of the response's body chunks.
pub struct ArrowStreamResponse<BodyStreamType> {
    query_job_id: QueryJobId,
    body: BodyStreamType,
}

impl<BodyStreamType> ArrowStreamResponse<BodyStreamType> {
    #[must_use]
    pub const fn new(query_job_id: QueryJobId, body: BodyStreamType) -> Self {
        Self { query_job_id, body }
    }
}

impl<BodyStreamType, ErrorType> IntoResponse for ArrowStreamResponse<BodyStreamType>
where
    BodyStreamType: Stream<Item = Result<Bytes, ErrorType>> + Send + 'static,
    ErrorType: Into<BoxError>,
{
    fn into_response(self) -> Response {
        (
            [
                (
                    CONTENT_TYPE,
                    HeaderValue::from_static(ARROW_STREAM_CONTENT_TYPE),
                ),
                (CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            ],
            [(QUERY_JOB_ID_HEADER, HeaderValue::from(self.query_job_id))],
            Body::from_stream(self.body),
        )
            .into_response()
    }
}

/// # Returns
///
/// The schema message that starts every Arrow IPC stream of results.
#[must_use]
pub fn schema_message() -> Bytes {
    static SCHEMA_MESSAGE: LazyLock<Bytes> = LazyLock::new(|| {
        let write_options = write_options();
        frame(
            IpcDataGenerator::default().schema_to_bytes_with_dictionary_tracker(
                &RESULT_SCHEMA,
                &mut DictionaryTracker::new(false),
                &write_options,
            ),
            &write_options,
        )
    });
    SCHEMA_MESSAGE.clone()
}

/// Encodes the IPC stream that follows a result stream's end-of-stream marker.
///
/// The stream holds one row with the search's terminal status and statistics, all null if the
/// search's query job couldn't be tracked to its end.
///
/// # Returns
///
/// The bytes of the summary stream, including its end-of-stream marker.
///
/// # Panics
///
/// Panics if encoding the summary fails, which it never does.
#[must_use]
pub fn summary_stream(outcome: Option<&SessionOutcome>) -> Bytes {
    static SUMMARY_SCHEMA: LazyLock<Arc<Schema>> = LazyLock::new(|| {
        Arc::new(Schema::new(vec![
            Field::new("status", DataType::Utf8, true),
            Field::new("num_results_emitted", DataType::UInt64, true),
            Field::new("num_duplicates_dropped", DataType::UInt64, true),
            Field::new("num_protocol_errors", DataType::UInt64, true),
            Field::new("num_incomplete_connections", DataType::UInt64, true),
        ]))
    });

    let status = outcome.and_then(|outcome| {
        serde_json::to_value(outcome.status)
            .ok()
            .and_then(|status| status.as_str().map(str::to_owned))
    });
    let count = |count: fn(&SessionOutcome) -> u64| -> ArrayRef {
        Arc::new(UInt64Array::from(vec![outcome.map(count)]))
    };
    let batch = RecordBatch::try_new(
        Arc::clone(&SUMMARY_SCHEMA),
        vec![
            Arc::new(StringArray::from(vec![status])),
            count(|outcome| outcome.stats.num_results_emitted),
            count(|outcome| outcome.stats.num_duplicates_dropped),
            count(|outcome| outcome.stats.num_protocol_errors),
            count(|outcome| outcome.stats.num_incomplete_connections),
        ],
    )
    .expect("the summary should match its schema");
    let mut stream = Vec::new();
    let mut writer =
        StreamWriter::try_new_with_options(&mut stream, &SUMMARY_SCHEMA, write_options())
            .expect("writing to a `Vec` shouldn't fail");
    writer
        .write(&batch)
        .expect("writing to a `Vec` shouldn't fail");
    writer.finish().expect("writing to a `Vec` shouldn't fail");
    drop(writer);
    Bytes::from(stream)
}

/// Encodes record batches of results whose archive IDs are all one archive's.
struct ResultIpcWriter {
    generator: IpcDataGenerator,
    tracker: DictionaryTracker,
    context: IpcWriteContext,

    /// The archive ID dictionary, which every record batch shares, so that the tracker never
    /// writes it again.
    archive_ids: ArrayRef,

    /// Zero keys that the archive ID columns of the record batches slice.
    zero_keys: Buffer,

    /// The framed replacement dictionary batch of the writer's archive.
    dictionary_message: Bytes,
}

impl ResultIpcWriter {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A writer of the results of the archive `archive_id`.
    ///
    /// # Panics
    ///
    /// Panics if encoding the archive ID dictionary fails, which it never does.
    fn new(archive_id: &str) -> Self {
        let mut writer = Self {
            generator: IpcDataGenerator::default(),
            tracker: DictionaryTracker::new(false),
            context: IpcWriteContext::default(),
            archive_ids: Arc::new(StringArray::from(vec![archive_id])),
            zero_keys: Buffer::from_vec(Vec::<i32>::new()),
            dictionary_message: Bytes::new(),
        };
        // Registers the dictionary of the archive ID column, as a stream writer does.
        let _ = writer.generator.schema_to_bytes_with_dictionary_tracker(
            &RESULT_SCHEMA,
            &mut writer.tracker,
            &write_options(),
        );
        let (dictionaries, _) = writer
            .encode(
                Arc::new(Int64Array::from(Vec::<i64>::new())),
                Arc::new(StringArray::from(Vec::<&str>::new())),
            )
            .expect("encoding the archive ID dictionary shouldn't fail");
        let [dictionary] = <[EncodedData; 1]>::try_from(dictionaries)
            .unwrap_or_else(|_| panic!("the result schema should have exactly one dictionary"));
        writer.dictionary_message = frame(dictionary, &write_options());
        writer
    }

    /// Encodes the results with `timestamps` and `messages` as one record batch.
    ///
    /// # Returns
    ///
    /// The framed record batch message, without the dictionary message, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::encode`]'s return values on failure.
    fn write(&mut self, timestamps: ArrayRef, messages: ArrayRef) -> Result<Bytes, ArrowError> {
        let (_, record_batch) = self.encode(timestamps, messages)?;
        Ok(frame(record_batch, &write_options()))
    }

    /// Encodes the results with `timestamps` and `messages` as one record batch.
    ///
    /// # Returns
    ///
    /// The dictionary batches the tracker hasn't written yet, and the record batch, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`DictionaryArray::try_new`]'s return values on failure.
    /// * Forwards [`RecordBatch::try_new`]'s return values on failure.
    /// * Forwards [`IpcDataGenerator::encode`]'s return values on failure.
    fn encode(
        &mut self,
        timestamps: ArrayRef,
        messages: ArrayRef,
    ) -> Result<(Vec<EncodedData>, EncodedData), ArrowError> {
        let num_rows = timestamps.len();
        if self.zero_keys.len() < num_rows * size_of::<i32>() {
            self.zero_keys = Buffer::from_vec(vec![0_i32; num_rows.next_power_of_two()]);
        }
        let keys = Int32Array::new(ScalarBuffer::new(self.zero_keys.clone(), 0, num_rows), None);
        let archive_ids: ArrayRef = Arc::new(DictionaryArray::<Int32Type>::try_new(
            keys,
            Arc::clone(&self.archive_ids),
        )?);
        let batch = RecordBatch::try_new(
            Arc::clone(&RESULT_SCHEMA),
            vec![archive_ids, timestamps, messages],
        )?;
        self.generator.encode(
            &batch,
            &mut self.tracker,
            &write_options(),
            &mut self.context,
        )
    }
}

/// The alignment of the buffers of the encoded IPC messages: the 8 bytes that the IPC format
/// requires, rather than arrow-rs's default of 64, which keeps small batches compact.
const IPC_ALIGNMENT: usize = 8;

/// # Returns
///
/// The options of every IPC message the server encodes.
///
/// # Panics
///
/// Panics if the options are invalid, which they never are.
fn write_options() -> IpcWriteOptions {
    IpcWriteOptions::try_new(IPC_ALIGNMENT, false, MetadataVersion::V5)
        .expect("the IPC write options should be valid")
}

/// Frames an encoded message as in the IPC streaming format.
///
/// # Returns
///
/// The framed message.
///
/// # Panics
///
/// Panics if writing to a `Vec` fails, which it never does.
fn frame(encoded: EncodedData, write_options: &IpcWriteOptions) -> Bytes {
    let mut framed = Vec::with_capacity(encoded.ipc_message.len() + encoded.arrow_data.len() + 16);
    arrow_ipc::writer::write_message(&mut framed, encoded, write_options)
        .expect("writing to a `Vec` shouldn't fail");
    Bytes::from(framed)
}
