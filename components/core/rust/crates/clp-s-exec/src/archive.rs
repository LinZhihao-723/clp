//! Opened archives.

use std::sync::Arc;
use std::sync::OnceLock;

use clp_s_format::FormatVersion;
use clp_s_format::TableInfo;
use clp_s_format::TimestampRange;
use clp_s_io::ArchiveStorage;
use clp_s_io::MemorySource;
use clp_s_io::ResolvedSource;
use clp_s_json::RenderContext;
use clp_s_resources::CancellationToken;
use clp_s_table::ArchiveTables;

use crate::ArchiveId;
use crate::ArchiveLocation;
use crate::ReadError;
use crate::ReaderContext;
use crate::Scan;
use crate::ScanRequest;
use crate::topk::TimestampBound;

/// Inclusive epoch-millisecond bounds of an archive's authoritative timestamps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimestampRangeMs {
    pub begin: i64,
    pub end: i64,
}

/// What is known about an opened archive without decompressing anything.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveInfo {
    pub archive_id: ArchiveId,
    pub version: FormatVersion,

    /// The number of records, from the table metadata.
    pub num_log_events: u64,

    /// Whether the archive records log order.
    pub has_log_order: bool,

    /// The range of the authoritative timestamps, or `None` if the archive has none.
    pub timestamp_range_ms: Option<TimestampRangeMs>,

    /// The size of the archive file in bytes.
    pub file_size: u64,
}

/// An opened archive: validated, immutable metadata and schema layer, shared across scans.
#[derive(Clone, Debug)]
pub struct Archive {
    inner: Arc<Inner>,
}

impl Archive {
    /// Factory function.
    ///
    /// Reads and validates the header, the metadata section and the schema layer. Archives this
    /// round cannot read (multi-file, structured arrays, versions outside [0.5.0, 0.5.x]) fail
    /// here, before any result is produced.
    ///
    /// # Returns
    ///
    /// The opened archive on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`clp_s_io::ResolvedSource::from_path`]'s return values on failure (a directory
    ///   is a multi-file archive).
    /// * Forwards [`clp_s_io::ArchiveStorage::open`]'s return values on failure.
    pub fn open(
        context: Arc<ReaderContext>,
        archive_id: ArchiveId,
        location: ArchiveLocation,
    ) -> Result<Self, ReadError> {
        let _span = tracing::debug_span!("clp_s.open", archive_id = % archive_id).entered();
        let source = match location {
            ArchiveLocation::File(path) => ResolvedSource::from_path(&path)?,
            ArchiveLocation::Memory(bytes) => ResolvedSource::Memory(MemorySource::new(bytes)),
            ArchiveLocation::Source(source) => ResolvedSource::Dyn(source),
        };
        let storage = ArchiveStorage::open(
            source,
            &context.config().open_options,
            &CancellationToken::new(),
        )?;
        let index = storage.index();
        let metadata = index.metadata();
        let timestamp_range_ms = metadata
            .timestamp_index()
            .authoritative()
            .and_then(|entry| match entry.range? {
                TimestampRange::EpochMillis { begin, end } => Some(TimestampRangeMs { begin, end }),
                _ => None,
            });
        let info = ArchiveInfo {
            archive_id,
            version: metadata.version().version,
            num_log_events: index.tables().num_log_events(),
            has_log_order: index.has_log_order(),
            timestamp_range_ms,
            file_size: metadata.header().compressed_size,
        };
        let tables = Arc::new(ArchiveTables::new(
            Arc::clone(index),
            Arc::clone(storage.dictionaries()),
        ));
        Ok(Self {
            inner: Arc::new(Inner {
                context,
                info,
                storage,
                tables,
                render_context: OnceLock::new(),
            }),
        })
    }

    /// # Returns
    ///
    /// What is known about the archive.
    #[must_use]
    pub fn info(&self) -> &ArchiveInfo {
        &self.inner.info
    }

    /// # Returns
    ///
    /// Every ERT in table-directory order, without decompressing anything.
    #[must_use]
    pub fn tables(&self) -> &[TableInfo] {
        self.inner.storage.index().tables().tables()
    }

    /// Plans `request` and opens a pull cursor over the candidate tables; under
    /// [`crate::ResultOrder::LatestFirst`] the cursor delivers the archive's top-k as one batch.
    ///
    /// # Returns
    ///
    /// The cursor on success; a pruned archive yields a cursor with no batch.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Scan::start`]'s return values on failure.
    pub fn scan(
        &self,
        request: Arc<ScanRequest>,
        cancellation_token: CancellationToken,
    ) -> Result<Scan, ReadError> {
        Scan::start(self.clone(), request, cancellation_token, None)
    }

    /// # Returns
    ///
    /// The context the archive was opened with.
    pub(crate) fn context(&self) -> &Arc<ReaderContext> {
        &self.inner.context
    }

    /// # Returns
    ///
    /// The archive's storage.
    pub(crate) fn storage(&self) -> &ArchiveStorage {
        &self.inner.storage
    }

    /// # Returns
    ///
    /// The archive's table layer.
    pub(crate) fn archive_tables(&self) -> &Arc<ArchiveTables> {
        &self.inner.tables
    }

    /// # Returns
    ///
    /// What the timestamp index says about the archive's authoritative timestamps.
    pub(crate) fn timestamp_bound(&self) -> TimestampBound {
        let Some(entry) = self
            .inner
            .storage
            .index()
            .metadata()
            .timestamp_index()
            .authoritative()
        else {
            return TimestampBound::Absent;
        };
        match entry.range {
            Some(TimestampRange::EpochMillis { end, .. }) => TimestampBound::AtMostMs(end),
            Some(_) | None => TimestampBound::Unknown,
        }
    }

    /// # Returns
    ///
    /// The archive's render caches, created on first use.
    pub(crate) fn render_context(&self) -> &RenderContext {
        self.inner
            .render_context
            .get_or_init(|| RenderContext::new(Arc::clone(&self.inner.tables)))
    }
}

/// The shared state of an opened archive.
#[derive(Debug)]
struct Inner {
    context: Arc<ReaderContext>,
    info: ArchiveInfo,
    storage: ArchiveStorage,
    tables: Arc<ArchiveTables>,
    render_context: OnceLock<RenderContext>,
}
