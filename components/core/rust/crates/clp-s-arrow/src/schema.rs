//! The Arrow schema of search results.

use std::sync::Arc;
use std::sync::LazyLock;

use arrow_schema::DataType;
use arrow_schema::Field;
use arrow_schema::Schema;
use arrow_schema::SchemaRef;

/// The name of the column that holds each result's archive ID.
pub const ARCHIVE_ID_COLUMN: &str = "archive_id";

/// The name of the column that holds each result's timestamp, in epoch milliseconds (0 for a
/// record without timestamp).
pub const TIMESTAMP_COLUMN: &str = "timestamp";

/// The name of the column that holds each result's record as JSON text, without a trailing
/// newline.
pub const MESSAGE_COLUMN: &str = "message";

/// The schema of every result batch. The archive ID column is dictionary-encoded, since the
/// results of one stream share one archive; its dictionary is the stream's first, so its ID is 0.
pub static RESULT_SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(Schema::new(vec![
        Field::new_dictionary(ARCHIVE_ID_COLUMN, DataType::Int32, DataType::Utf8, false),
        Field::new(TIMESTAMP_COLUMN, DataType::Int64, false),
        Field::new(MESSAGE_COLUMN, DataType::Utf8, false),
    ]))
});
