//! Field names of results-cache search documents (`archive_constants.hpp`,
//! `ResultsCacheOutputHandler::append_result`).

/// A field of a results-cache search-result document:
/// `{_id: {archive_id, log_event_idx}, orig_file_path, message, timestamp, dataset}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResultsCacheField {
    Id,
    ArchiveId,
    LogEventIdx,
    OrigFilePath,
    Message,
    Timestamp,
    Dataset,
}

impl ResultsCacheField {
    /// # Returns
    ///
    /// The document key of the field.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Id => "_id",
            Self::ArchiveId => "archive_id",
            Self::LogEventIdx => "log_event_idx",
            Self::OrigFilePath => "orig_file_path",
            Self::Message => "message",
            Self::Timestamp => "timestamp",
            Self::Dataset => "dataset",
        }
    }
}
