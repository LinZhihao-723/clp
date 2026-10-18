//! Push sessions over many archives.

use std::collections::HashSet;
use std::sync::Arc;

use clp_s_resources::CancellationToken;
use clp_s_resources::ResourceError;
use clp_s_search::PruneReason;
use clp_s_search::SearchPlanner;
use clp_s_search::TerminationStage;

use crate::Archive;
use crate::ArchiveId;
use crate::ArchiveLocation;
use crate::PartialSink;
use crate::PartitionInfo;
use crate::PartitionKind;
use crate::PartitionSummary;
use crate::ReadError;
use crate::ReaderContext;
use crate::RecordBatch;
use crate::ResultOrder;
use crate::Scan;
use crate::ScanRequest;
use crate::ScanStats;
use crate::Sink;
use crate::SinkControl;
use crate::topk::TimestampBound;
use crate::topk::TopK;

/// One request over many archives, with one sink lifecycle and one summary per archive.
///
/// Archives are searched in the order they were added, except under
/// [`crate::ResultOrder::LatestFirst`], where they are sorted by end timestamp, newest first, so
/// top-k pruning skips the most archives.
#[derive(Debug)]
pub struct SearchSession {
    context: Arc<ReaderContext>,
    request: Arc<ScanRequest>,
    archives: Vec<SessionArchive>,
    cancellation_token: CancellationToken,
    is_failed_archive_skipped: bool,
}

impl SearchSession {
    /// # Returns
    ///
    /// A builder of a session running `request` with the resources of `context`.
    #[must_use]
    pub fn builder(context: Arc<ReaderContext>, request: Arc<ScanRequest>) -> SearchSessionBuilder {
        SearchSessionBuilder {
            session: Self {
                context,
                request,
                archives: Vec::new(),
                cancellation_token: CancellationToken::new(),
                is_failed_archive_skipped: false,
            },
        }
    }

    /// Runs the session: opens one partition per archive (plus the top-k partition under
    /// [`crate::ResultOrder::LatestFirst`]), pushes every batch into it, and merges the partition
    /// outputs.
    ///
    /// Under [`crate::ResultOrder::LatestFirst`] the timestamp bound of every archive is read
    /// first; the archive partitions receive no batch, archives whose timestamps cannot beat the
    /// current top-k are skipped unread, and the winners are delivered once to the top-k partition.
    ///
    /// # Type Parameters
    ///
    /// * `SinkType` - The sink receiving the session's records.
    ///
    /// # Returns
    ///
    /// The merged sink output and one summary per archive, in the order the archives were added,
    /// on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ReadError::InvalidArgument`] if the session holds `u32::MAX` archives or more.
    /// * Forwards [`ScanRequest::with_requirements`]'s return values on failure.
    /// * Forwards [`ScanRequest::search_planner`]'s return values on failure.
    /// * Forwards [`Self::run_in_order`]'s return values on failure.
    /// * Forwards [`Self::run_top_k`]'s return values on failure.
    pub fn run<SinkType: Sink>(
        self,
        sink: Arc<SinkType>,
    ) -> Result<SessionReport<SinkType::Output>, ReadError> {
        let num_archives = u32::try_from(self.archives.len()).unwrap_or(u32::MAX);
        if u32::MAX == num_archives {
            return Err(ReadError::InvalidArgument(
                "a session holds fewer than u32::MAX archives".into(),
            ));
        }
        let request = Arc::new(self.request.with_requirements(sink.requirements())?);
        let planner = if request.requires_search_planner() {
            Some(request.search_planner()?)
        } else {
            None
        };
        let report = if let ResultOrder::LatestFirst { limit } = request.order() {
            self.run_top_k(&request, planner.as_ref(), TopK::new(limit), &sink)?
        } else {
            self.run_in_order(&request, planner.as_ref(), &sink)?
        };
        drop(sink);
        Ok(report)
    }

    /// Runs the session in archive order, pushing every batch into its archive's partition.
    ///
    /// # Type Parameters
    ///
    /// * `SinkType` - The sink receiving the session's records.
    ///
    /// # Returns
    ///
    /// The merged sink output and one summary per archive, in archive order, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`CancellationToken::check`]'s return values on failure.
    /// * Forwards [`Sink::open_partial`]'s return values on failure.
    /// * Forwards [`PartitionRun::execute`]'s return values on failure.
    /// * Forwards [`Self::settle_archive`]'s return values on failure.
    fn run_in_order<SinkType: Sink>(
        self,
        request: &Arc<ScanRequest>,
        planner: Option<&SearchPlanner>,
        sink: &Arc<SinkType>,
    ) -> Result<SessionReport<SinkType::Output>, ReadError> {
        let mut output = SinkType::Output::default();
        let mut archive_summaries = Vec::with_capacity(self.archives.len());
        let mut is_stopped = false;
        for (index, archive) in (0_u32..).zip(self.archives) {
            if is_stopped {
                archive_summaries.push(ArchiveSummary::stopped(archive.archive_id));
                continue;
            }
            self.cancellation_token.check()?;
            let partition = PartitionInfo {
                index,
                kind: PartitionKind::Archive(archive.archive_id.clone()),
                order: request.order(),
            };
            let mut partial = sink.open_partial(&partition)?;
            let run = PartitionRun::execute(
                &self.context,
                request,
                planner,
                archive.archive_id.clone(),
                archive.location,
                &self.cancellation_token,
                &mut partial,
            )?;
            let (summary, control) = Self::settle_archive(
                &self.cancellation_token,
                self.is_failed_archive_skipped,
                sink,
                &mut output,
                partial,
                archive.archive_id,
                run,
            )?;
            is_stopped = SinkControl::StopSession == control;
            archive_summaries.push(summary);
        }
        Ok(SessionReport {
            output,
            archive_summaries,
        })
    }

    /// Runs the session under [`ResultOrder::LatestFirst`]: reads the timestamp bound of every
    /// archive, offers the matches of the archives (sorted by end timestamp, newest first) to
    /// `top_k`, and delivers the winners to the top-k partition.
    ///
    /// # Type Parameters
    ///
    /// * `SinkType` - The sink receiving the session's records.
    ///
    /// # Returns
    ///
    /// The merged sink output and one summary per archive, in the order the archives were added,
    /// on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::read_timestamp_bounds`]'s return values on failure.
    /// * Forwards [`CancellationToken::check`]'s return values on failure.
    /// * Forwards [`Sink::open_partial`]'s return values on failure.
    /// * Forwards [`Self::settle_archive`]'s return values on failure.
    /// * Forwards [`Self::deliver_top_k`]'s return values on failure.
    fn run_top_k<SinkType: Sink>(
        self,
        request: &Arc<ScanRequest>,
        planner: Option<&SearchPlanner>,
        mut top_k: TopK,
        sink: &Arc<SinkType>,
    ) -> Result<SessionReport<SinkType::Output>, ReadError> {
        let mut bounded = Self::read_timestamp_bounds(
            &self.context,
            self.archives,
            &self.cancellation_token,
            self.is_failed_archive_skipped,
        )?;
        bounded.sort_by_key(TopKArchive::search_rank);

        let mut output = SinkType::Output::default();
        let mut archive_summaries = Vec::with_capacity(bounded.len());
        let mut is_stopped = false;
        let mut num_partitions = 0_u32;
        for TopKArchive {
            position,
            archive_id,
            archive,
            bound,
        } in bounded
        {
            if is_stopped {
                archive_summaries.push((position, ArchiveSummary::stopped(archive_id)));
                continue;
            }
            self.cancellation_token.check()?;
            let partition = PartitionInfo {
                index: num_partitions,
                kind: PartitionKind::Archive(archive_id.clone()),
                order: request.order(),
            };
            num_partitions += 1;
            let partial = sink.open_partial(&partition)?;
            let run = match archive.open_unless_excluded(&self.context, &archive_id, bound, &top_k)
            {
                Ok(Some(archive)) => PartitionRun::offer_to_top_k(
                    request,
                    planner,
                    archive,
                    &self.cancellation_token,
                    &mut top_k,
                ),
                Ok(None) => PartitionRun::excluded_by_top_k(),
                Err(error) => PartitionRun::failed(error),
            };
            let (summary, control) = Self::settle_archive(
                &self.cancellation_token,
                self.is_failed_archive_skipped,
                sink,
                &mut output,
                partial,
                archive_id,
                run,
            )?;
            is_stopped = SinkControl::StopSession == control;
            archive_summaries.push((position, summary));
        }
        archive_summaries.sort_by_key(|(position, _)| *position);
        let archive_summaries = archive_summaries
            .into_iter()
            .map(|(_, summary)| summary)
            .collect();
        if is_stopped {
            return Ok(SessionReport {
                output,
                archive_summaries,
            });
        }

        self.cancellation_token.check()?;
        Self::deliver_top_k(request, top_k, num_partitions, sink, &mut output)?;
        Ok(SessionReport {
            output,
            archive_summaries,
        })
    }

    /// Opens every archive of a top-k session, in the order they were added, to read its
    /// timestamp bound.
    ///
    /// A session of more than [`MAX_NUM_ARCHIVES_HELD_OPEN`] archives closes each archive once its
    /// bound is read, to be reopened when its turn comes, so that the session never holds more
    /// open archives (and file handles) than that.
    ///
    /// # Returns
    ///
    /// The archives, failed ones included, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ReadError::Resource`] ([`ResourceError::Cancelled`]) if an archive fails to open once
    ///   `cancellation_token` is cancelled.
    /// * [`ReadError::InArchive`] if an archive fails to open and failed archives are not skipped.
    /// * Forwards [`CancellationToken::check`]'s return values on failure.
    fn read_timestamp_bounds(
        context: &Arc<ReaderContext>,
        archives: Vec<SessionArchive>,
        cancellation_token: &CancellationToken,
        is_failed_archive_skipped: bool,
    ) -> Result<Vec<TopKArchive>, ReadError> {
        let is_held_open = archives.len() <= MAX_NUM_ARCHIVES_HELD_OPEN;
        let mut bounded = Vec::with_capacity(archives.len());
        for (position, archive) in archives.into_iter().enumerate() {
            cancellation_token.check()?;
            let reopened_location = (!is_held_open).then(|| archive.location.clone());
            let (pending, bound) = match Archive::open(
                Arc::clone(context),
                archive.archive_id.clone(),
                archive.location,
            ) {
                Ok(opened) => {
                    let bound = opened.timestamp_bound();
                    let pending = reopened_location
                        .map_or_else(|| PendingArchive::Open(opened), PendingArchive::Closed);
                    (pending, bound)
                }
                Err(_) if cancellation_token.is_cancelled() => {
                    return Err(ResourceError::Cancelled.into());
                }
                Err(error) if !is_failed_archive_skipped => {
                    return Err(error.in_archive(archive.archive_id));
                }
                Err(error) => (PendingArchive::Failed(error), TimestampBound::Unknown),
            };
            bounded.push(TopKArchive {
                position,
                archive_id: archive.archive_id,
                archive: pending,
                bound,
            });
        }
        Ok(bounded)
    }

    /// Delivers the winners of `top_k` as one gathered batch to the top-k partition, which is
    /// partition `index`, and merges its output.
    ///
    /// # Type Parameters
    ///
    /// * `SinkType` - The sink receiving the session's records.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Sink::open_partial`]'s return values on failure.
    /// * Forwards [`TopK::into_batch`]'s return values on failure.
    /// * Forwards [`PartialSink::consume`]'s return values on failure.
    /// * Forwards [`PartialSink::finish`]'s return values on failure.
    /// * Forwards [`Sink::merge`]'s return values on failure.
    fn deliver_top_k<SinkType: Sink>(
        request: &ScanRequest,
        top_k: TopK,
        index: u32,
        sink: &Arc<SinkType>,
        output: &mut SinkType::Output,
    ) -> Result<(), ReadError> {
        let partition = PartitionInfo {
            index,
            kind: PartitionKind::TopK,
            order: request.order(),
        };
        let mut partial = sink.open_partial(&partition)?;
        let winners = top_k.into_batch()?;
        if 0 != winners.num_records() {
            partial.consume(&RecordBatch::from_gathered(&winners, request.outputs()))?;
        }
        let summary = PartitionSummary {
            kind: PartitionKind::TopK,
            termination_stage: None,
            prune_reason: None,
            stats: ScanStats::default(),
        };
        let partial_output = partial.finish(&summary)?;
        sink.merge(output, partial_output)?;
        Ok(())
    }

    /// Finishes one archive partition: fails the session on an archive failure (unless failed
    /// archives are skipped), finishes the partial sink and merges its output.
    ///
    /// # Type Parameters
    ///
    /// * `SinkType` - The sink receiving the session's records.
    ///
    /// # Returns
    ///
    /// The archive's summary and what the session does next on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ReadError::Resource`] ([`ResourceError::Cancelled`]) if the archive failed once
    ///   `cancellation_token` is cancelled.
    /// * [`ReadError::InArchive`] if the archive failed and failed archives are not skipped.
    /// * Forwards [`PartialSink::finish`]'s return values on failure.
    /// * Forwards [`Sink::merge`]'s return values on failure.
    fn settle_archive<SinkType: Sink>(
        cancellation_token: &CancellationToken,
        is_failed_archive_skipped: bool,
        sink: &Arc<SinkType>,
        output: &mut SinkType::Output,
        partial: SinkType::Partial,
        archive_id: ArchiveId,
        mut run: PartitionRun,
    ) -> Result<(ArchiveSummary, SinkControl), ReadError> {
        if run.error.is_some() && cancellation_token.is_cancelled() {
            return Err(ResourceError::Cancelled.into());
        }
        if let Some(error) = run.error.take() {
            if !is_failed_archive_skipped {
                return Err(error.in_archive(archive_id));
            }
            tracing::warn!(
                archive_id = % archive_id,
                error = % error,
                "Skipping a failed archive."
            );
            run.error = Some(error);
        }
        let summary = PartitionSummary {
            kind: PartitionKind::Archive(archive_id.clone()),
            termination_stage: run.termination_stage,
            prune_reason: run.prune_reason,
            stats: run.stats,
        };
        let partial_output = partial.finish(&summary)?;
        let control = sink.merge(output, partial_output)?;
        let control = if run.is_session_stopped {
            SinkControl::StopSession
        } else {
            control
        };
        Ok((
            ArchiveSummary {
                archive_id,
                termination_stage: run.termination_stage,
                prune_reason: run.prune_reason,
                stats: run.stats,
                error: run.error,
            },
            control,
        ))
    }
}

/// Builds a [`SearchSession`].
#[derive(Debug)]
pub struct SearchSessionBuilder {
    session: SearchSession,
}

impl SearchSessionBuilder {
    /// # Returns
    ///
    /// The builder with one more archive to search.
    #[must_use]
    pub fn archive(mut self, archive_id: ArchiveId, location: ArchiveLocation) -> Self {
        self.session.archives.push(SessionArchive {
            archive_id,
            location,
        });
        self
    }

    /// # Returns
    ///
    /// The builder with `cancellation_token` as the session's token.
    #[must_use]
    pub fn cancellation_token(mut self, cancellation_token: CancellationToken) -> Self {
        self.session.cancellation_token = cancellation_token;
        self
    }

    /// # Returns
    ///
    /// The builder set to record a failing archive in its summary and continue
    /// (`is_failed_archive_skipped`), instead of failing the session.
    #[must_use]
    pub const fn skip_failed_archives(mut self, is_failed_archive_skipped: bool) -> Self {
        self.session.is_failed_archive_skipped = is_failed_archive_skipped;
        self
    }

    /// Builds the session.
    ///
    /// # Returns
    ///
    /// The session on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ReadError::InvalidArgument`] if two archives share an id.
    pub fn build(self) -> Result<SearchSession, ReadError> {
        let mut archive_ids = HashSet::with_capacity(self.session.archives.len());
        for archive in &self.session.archives {
            if !archive_ids.insert(&archive.archive_id) {
                return Err(ReadError::InvalidArgument(
                    format!("archive id `{}` is given twice", archive.archive_id).into(),
                ));
            }
        }
        Ok(self.session)
    }
}

/// The merged sink output and one summary per archive, in archive order.
#[derive(Debug)]
pub struct SessionReport<SinkOutputType> {
    pub output: SinkOutputType,
    pub archive_summaries: Vec<ArchiveSummary>,
}

/// What happened to one archive of a session.
#[non_exhaustive]
#[derive(Debug)]
pub struct ArchiveSummary {
    pub archive_id: ArchiveId,

    /// The stage at which the search ended, or `None` if the archive failed to open.
    pub termination_stage: Option<TerminationStage>,

    /// Why planning proved the archive empty, if it did (for example DV-20's diagnostic).
    pub prune_reason: Option<PruneReason>,

    pub stats: ScanStats,

    /// The archive's error, when failed archives are skipped.
    pub error: Option<ReadError>,
}

impl ArchiveSummary {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The summary of an archive skipped because a sink stopped the session.
    fn stopped(archive_id: ArchiveId) -> Self {
        Self {
            archive_id,
            termination_stage: Some(TerminationStage::StoppedBySink),
            prune_reason: None,
            stats: ScanStats::default(),
            error: None,
        }
    }
}

/// The largest top-k session whose archives stay open from the time their timestamp bounds are
/// read until they are searched. Larger sessions reopen each archive, which repeats the reads of
/// its header and metadata but keeps the number of open file handles bounded.
const MAX_NUM_ARCHIVES_HELD_OPEN: usize = 64;

/// One archive of a session.
#[derive(Debug)]
struct SessionArchive {
    archive_id: ArchiveId,
    location: ArchiveLocation,
}

/// One archive of a session under [`ResultOrder::LatestFirst`], whose timestamp bound is read
/// before any archive is searched.
#[derive(Debug)]
struct TopKArchive {
    /// The position at which the archive was added to the session.
    position: usize,

    archive_id: ArchiveId,
    archive: PendingArchive,
    bound: TimestampBound,
}

impl TopKArchive {
    /// # Returns
    ///
    /// The rank of the archive in search order: archives with a known end timestamp, newest
    /// first; then archives with unknown timestamps; then archives without timestamps; then
    /// archives that failed to open. Ties keep the order in which the archives were added.
    const fn search_rank(&self) -> (u8, std::cmp::Reverse<i64>, usize) {
        let (group, end) = match (&self.archive, self.bound) {
            (PendingArchive::Failed(_), _) => (3, i64::MIN),
            (_, TimestampBound::AtMostMs(end)) => (0, end),
            (_, TimestampBound::Unknown) => (1, i64::MIN),
            (_, TimestampBound::Absent) => (2, i64::MIN),
        };
        (group, std::cmp::Reverse(end), self.position)
    }
}

/// A top-k archive waiting for its turn in the search.
#[derive(Debug)]
enum PendingArchive {
    Open(Archive),

    /// An archive closed after its timestamp bound was read, to be reopened from its location.
    Closed(ArchiveLocation),

    /// The error the archive failed to open with.
    Failed(ReadError),
}

impl PendingArchive {
    /// Opens the archive `archive_id` for its turn in the search, unless `top_k` shows from its
    /// timestamp `bound` that none of its records can be kept.
    ///
    /// # Returns
    ///
    /// The opened archive on success, or `None` if `top_k` excludes it.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * The error the archive failed to open with when its bound was read.
    /// * Forwards [`Archive::open`]'s return values on failure.
    fn open_unless_excluded(
        self,
        context: &Arc<ReaderContext>,
        archive_id: &ArchiveId,
        bound: TimestampBound,
        top_k: &TopK,
    ) -> Result<Option<Archive>, ReadError> {
        let location = match self {
            Self::Failed(error) => return Err(error),
            _ if top_k.excludes_archive(archive_id, bound) => return Ok(None),
            Self::Open(archive) => return Ok(Some(archive)),
            Self::Closed(location) => location,
        };
        Archive::open(Arc::clone(context), archive_id.clone(), location).map(Some)
    }
}

/// What happened to one archive partition.
#[derive(Debug, Default)]
struct PartitionRun {
    termination_stage: Option<TerminationStage>,
    prune_reason: Option<PruneReason>,
    stats: ScanStats,

    /// The archive's error, if it failed.
    error: Option<ReadError>,

    /// Whether the sink asked to stop the session.
    is_session_stopped: bool,
}

impl PartitionRun {
    /// Factory function.
    ///
    /// Opens, plans and scans one archive, pushing every batch into `partial`. Archive failures are
    /// reported in the run; sink failures fail the call.
    ///
    /// # Type Parameters
    ///
    /// * `PartialType` - The partition's partial sink.
    ///
    /// # Returns
    ///
    /// The run on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`PartialSink::consume`]'s return values on failure.
    fn execute<PartialType: PartialSink>(
        context: &Arc<ReaderContext>,
        request: &Arc<ScanRequest>,
        planner: Option<&SearchPlanner>,
        archive_id: ArchiveId,
        location: ArchiveLocation,
        cancellation_token: &CancellationToken,
        partial: &mut PartialType,
    ) -> Result<Self, ReadError> {
        let mut run = Self::default();
        let scan = Archive::open(Arc::clone(context), archive_id, location).and_then(|archive| {
            Scan::start(
                archive,
                Arc::clone(request),
                cancellation_token.clone(),
                planner,
            )
        });
        let mut scan = match scan {
            Ok(scan) => scan,
            Err(error) => {
                run.error = Some(error);
                return Ok(run);
            }
        };
        run.termination_stage = Some(scan.termination_stage());
        run.prune_reason = scan.prune_reason();
        loop {
            match scan.advance() {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => {
                    run.error = Some(error);
                    break;
                }
            }
            let Some(batch) = scan.current() else {
                break;
            };
            match partial.consume(&batch)? {
                SinkControl::Continue => {}
                SinkControl::StopArchive => {
                    run.termination_stage = Some(TerminationStage::StoppedBySink);
                    break;
                }
                SinkControl::StopSession => {
                    run.termination_stage = Some(TerminationStage::StoppedBySink);
                    run.is_session_stopped = true;
                    break;
                }
            }
        }
        run.stats = scan.stats();
        Ok(run)
    }

    /// Factory function.
    ///
    /// Offers every match of `archive` to `top_k`. Archive failures are reported in the run.
    ///
    /// # Returns
    ///
    /// The run.
    fn offer_to_top_k(
        request: &Arc<ScanRequest>,
        planner: Option<&SearchPlanner>,
        archive: Archive,
        cancellation_token: &CancellationToken,
        top_k: &mut TopK,
    ) -> Self {
        let mut run = Self::default();
        let mut scan = match Scan::start(
            archive,
            Arc::clone(request),
            cancellation_token.clone(),
            planner,
        ) {
            Ok(scan) => scan,
            Err(error) => {
                run.error = Some(error);
                return run;
            }
        };
        run.termination_stage = Some(scan.termination_stage());
        run.prune_reason = scan.prune_reason();
        if let Err(error) = scan.collect_top_k(top_k) {
            run.error = Some(error);
        }
        run.stats = scan.stats();
        run
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The run of an archive the top-k skipped unread because none of its records can be kept.
    fn excluded_by_top_k() -> Self {
        Self {
            termination_stage: Some(TerminationStage::TimeRangeMatching),
            prune_reason: Some(PruneReason::TimeRangeDisjoint),
            ..Self::default()
        }
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The run of an archive that failed with `error` before it was scanned.
    fn failed(error: ReadError) -> Self {
        Self {
            error: Some(error),
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clp_s_resources::CancellationToken;

    use super::MAX_NUM_ARCHIVES_HELD_OPEN;
    use super::PendingArchive;
    use super::SearchSession;
    use super::SessionArchive;
    use crate::ArchiveId;
    use crate::ArchiveLocation;
    use crate::ReaderContext;
    use crate::topk::TimestampBound;

    #[test]
    fn large_top_k_sessions_close_archives_once_their_bounds_are_read() -> anyhow::Result<()> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/archives/logs.ts.clps");
        let context = ReaderContext::builder().build()?;
        for (num_archives, is_held_open) in [
            (1, true),
            (MAX_NUM_ARCHIVES_HELD_OPEN, true),
            (MAX_NUM_ARCHIVES_HELD_OPEN + 1, false),
        ] {
            let archives = (0..num_archives)
                .map(|index| SessionArchive {
                    archive_id: ArchiveId::from(format!("{index}.clps")),
                    location: ArchiveLocation::File(path.clone()),
                })
                .collect();
            let bounded = SearchSession::read_timestamp_bounds(
                &context,
                archives,
                &CancellationToken::new(),
                false,
            )?;
            assert_eq!(bounded.len(), num_archives);
            for (position, archive) in bounded.iter().enumerate() {
                assert_eq!(archive.position, position);
                assert!(
                    matches!(archive.bound, TimestampBound::AtMostMs(_)),
                    "{archive:?}"
                );
                let is_open = match &archive.archive {
                    PendingArchive::Open(_) => true,
                    PendingArchive::Closed(ArchiveLocation::File(closed_path)) => {
                        assert_eq!(closed_path, &path);
                        false
                    }
                    pending @ (PendingArchive::Closed(_) | PendingArchive::Failed(_)) => {
                        anyhow::bail!("{num_archives} archives: unexpected {pending:?}")
                    }
                };
                assert_eq!(is_open, is_held_open, "{num_archives} archives");
            }
        }
        Ok(())
    }
}
