//! The sink protocol of push sessions (tantivy `Collector` shape).

use clp_s_resources::ErrorKind;
use clp_s_search::PruneReason;
use clp_s_search::TerminationStage;

use crate::ArchiveId;
use crate::OutputRequirements;
use crate::ReadError;
use crate::RecordBatch;
use crate::ResultOrder;
use crate::ScanStats;

/// What the engine does after a sink consumes a batch or merges a partition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinkControl {
    Continue,

    /// Skip the rest of the current archive; the session still ends with `Ok`.
    StopArchive,

    /// Skip the rest of the session; the session still ends with `Ok`.
    StopSession,
}

/// The order in which partition outputs are merged.
///
/// An enum rather than a `bool` because it is the return type of a trait other crates implement:
/// a keyed merge order can then be added without breaking them.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeOrder {
    /// Partitions may merge in completion order.
    Commutative,

    /// Partitions merge in session scan order.
    Scan,
}

/// The identity of one partition of a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionInfo {
    /// Position of the partition in the session's scan order.
    pub index: u32,

    pub kind: PartitionKind,

    /// The session's resolved result order.
    pub order: ResultOrder,
}

/// What a partition covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PartitionKind {
    /// The records of one archive.
    Archive(ArchiveId),

    /// The session-wide top-k winners, delivered once after every archive partition under
    /// [`ResultOrder::LatestFirst`] as one gathered batch sorted by [`crate::LatestFirstKey`].
    TopK,
}

/// What happened to one partition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionSummary {
    pub kind: PartitionKind,

    /// The stage at which the archive's search ended (`None` for the top-k partition).
    pub termination_stage: Option<TerminationStage>,

    /// Why planning proved the archive empty, if it did.
    pub prune_reason: Option<PruneReason>,

    pub stats: ScanStats,
}

/// A destination for the records of one session.
pub trait Sink: Send + Sync + 'static {
    type Partial: PartialSink;
    type Output: Default + Send + 'static;

    /// Declares what the engine must materialize, and any order the sink requires.
    ///
    /// # Returns
    ///
    /// The sink's requirements; the session merges them into the request.
    fn requirements(&self) -> OutputRequirements;

    /// Declares whether partition outputs may merge in completion order.
    ///
    /// # Returns
    ///
    /// The merge order.
    fn merge_order(&self) -> MergeOrder;

    /// Opens the partial sink of one partition.
    ///
    /// # Parameters
    ///
    /// * `partition` - The partition's identity and the session's resolved order.
    ///
    /// # Returns
    ///
    /// The partial sink on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`SinkError`] if the partial sink cannot be opened.
    fn open_partial(&self, partition: &PartitionInfo) -> Result<Self::Partial, SinkError>;

    /// Folds one finished partition into the session output.
    ///
    /// # Parameters
    ///
    /// * `output` - The accumulated session output.
    /// * `partial` - The next partition's output, in the order [`Sink::merge_order`] promises.
    ///
    /// # Returns
    ///
    /// What the session does next on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`SinkError`] if merging or one of its side effects fails.
    fn merge(
        &self,
        output: &mut Self::Output,
        partial: <Self::Partial as PartialSink>::Output,
    ) -> Result<SinkControl, SinkError>;
}

/// The per-partition consumer of batches.
pub trait PartialSink: Send + 'static {
    type Output: Send + 'static;

    /// Consumes one batch, which is valid only during the call.
    ///
    /// # Parameters
    ///
    /// * `batch` - A non-empty batch with everything the sink's requirements asked for.
    ///
    /// # Returns
    ///
    /// What the session does next on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`SinkError`] if writing fails.
    fn consume(&mut self, batch: &RecordBatch<'_>) -> Result<SinkControl, SinkError>;

    /// Finishes the partition; also called for archives pruned before any table was read.
    ///
    /// # Parameters
    ///
    /// * `summary` - What happened to the partition.
    ///
    /// # Returns
    ///
    /// The partition's output on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`SinkError`] if flushing fails.
    fn finish(self, summary: &PartitionSummary) -> Result<Self::Output, SinkError>;
}

/// Errors raised by sinks; the boxed variants keep the set of sinks open.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    #[error("sink i/o failed")]
    Io(#[from] std::io::Error),

    #[error("the downstream consumer closed the connection")]
    DownstreamClosed,

    #[error("results cache write failed")]
    Database(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("cannot encode a result: {0}")]
    Encode(Box<str>),

    #[error(transparent)]
    Read(Box<ReadError>),

    #[error("sink failed")]
    Custom(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl SinkError {
    /// # Returns
    ///
    /// The category this error maps onto; a wrapped read error keeps its own category.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::Read(error) => error.kind(),
            Self::Io(_)
            | Self::DownstreamClosed
            | Self::Database(_)
            | Self::Encode(_)
            | Self::Custom(_) => ErrorKind::Sink,
        }
    }
}
