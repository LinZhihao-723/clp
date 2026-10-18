//! The public error type of the reader.

use clp_s_io::StorageError;
use clp_s_json::JsonError;
use clp_s_query::QueryError;
use clp_s_resources::ErrorKind;
use clp_s_resources::ResourceError;
use clp_s_search::EvalError;
use clp_s_search::PlanError;
use clp_s_table::LayoutError;
use clp_s_table::TableError;

use crate::ArchiveId;
use crate::SinkError;

/// The errors of the public API; payloads are boxed so results stay small (at most 32 bytes,
/// asserted at compile time).
///
/// "No matches" is never an error, and an error never means "no rows".
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("invalid argument: {0}")]
    InvalidArgument(Box<str>),

    #[error(transparent)]
    Query(Box<QueryError>),

    #[error(transparent)]
    Storage(Box<StorageError>),

    #[error(transparent)]
    Layout(Box<LayoutError>),

    #[error(transparent)]
    Table(Box<TableError>),

    #[error(transparent)]
    Json(Box<JsonError>),

    #[error(transparent)]
    Plan(Box<PlanError>),

    #[error(transparent)]
    Eval(Box<EvalError>),

    #[error(transparent)]
    Resource(Box<ResourceError>),

    #[error(transparent)]
    Sink(Box<SinkError>),

    #[error("internal error: {0}")]
    Internal(Box<str>),

    #[error("archive {archive_id}: {source}")]
    InArchive {
        archive_id: ArchiveId,
        source: Box<Self>,
    },
}

impl ReadError {
    /// # Returns
    ///
    /// The category that C status codes, Python exceptions and CLI exit codes map from.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::InvalidArgument(_) => ErrorKind::InvalidArgument,
            Self::Query(error) => error.kind(),
            Self::Storage(error) => error.kind(),
            Self::Layout(error) => error.kind(),
            Self::Table(error) => error.kind(),
            Self::Json(error) => error.kind(),
            Self::Plan(error) => error.kind(),
            Self::Eval(error) => error.kind(),
            Self::Resource(error) => error.kind(),
            Self::Sink(error) => error.kind(),
            Self::Internal(_) => ErrorKind::Internal,
            Self::InArchive { source, .. } => source.kind(),
        }
    }

    /// # Returns
    ///
    /// This error wrapped with the archive it occurred in.
    #[must_use]
    pub fn in_archive(self, archive_id: ArchiveId) -> Self {
        Self::InArchive {
            archive_id,
            source: Box::new(self),
        }
    }
}

impl From<QueryError> for ReadError {
    fn from(error: QueryError) -> Self {
        Self::Query(Box::new(error))
    }
}

impl From<StorageError> for ReadError {
    fn from(error: StorageError) -> Self {
        Self::Storage(Box::new(error))
    }
}

impl From<LayoutError> for ReadError {
    fn from(error: LayoutError) -> Self {
        Self::Layout(Box::new(error))
    }
}

impl From<TableError> for ReadError {
    fn from(error: TableError) -> Self {
        Self::Table(Box::new(error))
    }
}

impl From<JsonError> for ReadError {
    fn from(error: JsonError) -> Self {
        Self::Json(Box::new(error))
    }
}

impl From<PlanError> for ReadError {
    fn from(error: PlanError) -> Self {
        Self::Plan(Box::new(error))
    }
}

impl From<EvalError> for ReadError {
    fn from(error: EvalError) -> Self {
        Self::Eval(Box::new(error))
    }
}

impl From<ResourceError> for ReadError {
    fn from(error: ResourceError) -> Self {
        Self::Resource(Box::new(error))
    }
}

impl From<SinkError> for ReadError {
    fn from(error: SinkError) -> Self {
        Self::Sink(Box::new(error))
    }
}

#[cfg(test)]
mod tests {
    use clp_s_io::StorageError;
    use clp_s_resources::ErrorKind;
    use clp_s_resources::ResourceError;

    use super::ReadError;
    use crate::ArchiveId;
    use crate::SinkError;

    #[test]
    fn kinds_delegate_through_every_wrapper() {
        let multi_file = ReadError::from(StorageError::MultiFileArchive {
            path: "archive".into(),
        });
        assert_eq!(multi_file.kind(), ErrorKind::UnsupportedArchive);
        let wrapped = multi_file.in_archive(ArchiveId::from("a.clps"));
        assert_eq!(wrapped.kind(), ErrorKind::UnsupportedArchive);
        assert_eq!(
            wrapped.to_string(),
            "archive a.clps: `archive` is a multi-file archive, which this reader does not support"
        );
        let cancelled = ReadError::from(ResourceError::Cancelled).in_archive(ArchiveId::from("b"));
        assert_eq!(cancelled.kind(), ErrorKind::Cancelled);
        let sink_read = ReadError::from(SinkError::Read(Box::new(ReadError::from(
            ResourceError::BudgetExhausted {
                requested_size: 2,
                limit_size: 1,
            },
        ))));
        assert_eq!(sink_read.kind(), ErrorKind::ResourceExhausted);
        assert_eq!(
            ReadError::from(SinkError::DownstreamClosed).kind(),
            ErrorKind::Sink
        );
        assert_eq!(
            ReadError::Internal("bug".into()).kind(),
            ErrorKind::Internal
        );
    }
}
