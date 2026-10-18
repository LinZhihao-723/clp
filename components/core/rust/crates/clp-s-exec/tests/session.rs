//! Push sessions over C++ fixture archives: partition lifecycle, archive order, requirement
//! merging, sink control, failed archives and cancellation.

use std::num::NonZeroU32;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use clp_s_exec::ArchiveId;
use clp_s_exec::ArchiveLocation;
use clp_s_exec::MergeOrder;
use clp_s_exec::OutputRequirements;
use clp_s_exec::PartialSink;
use clp_s_exec::PartitionInfo;
use clp_s_exec::PartitionKind;
use clp_s_exec::PartitionSummary;
use clp_s_exec::ReadError;
use clp_s_exec::ReaderContext;
use clp_s_exec::RecordBatch;
use clp_s_exec::ResultOrder;
use clp_s_exec::ScanRequest;
use clp_s_exec::SearchSession;
use clp_s_exec::SessionReport;
use clp_s_exec::Sink;
use clp_s_exec::SinkControl;
use clp_s_exec::SinkError;
use clp_s_resources::CancellationToken;
use clp_s_resources::ErrorKind;
use clp_s_search::TerminationStage;

/// # Returns
///
/// The path of fixture archive `name`.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/archives")
        .join(format!("{name}.clps"))
}

/// What one partition saw.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PartitionTrace {
    info: PartitionInfo,
    num_batches: usize,
    num_records: usize,
    summary: Option<PartitionSummary>,
}

/// A recording sink with configurable requirements and control decisions.
#[derive(Debug)]
struct TraceSink {
    requirements: OutputRequirements,

    /// After how many batches (counted per partition) `control` is returned.
    num_batches_before_control: usize,

    control: SinkControl,
}

impl TraceSink {
    /// A sink that asks for JSON and always continues.
    const JSON: Self = Self {
        requirements: OutputRequirements::json(),
        num_batches_before_control: usize::MAX,
        control: SinkControl::Continue,
    };
}

impl Sink for TraceSink {
    type Output = Vec<PartitionTrace>;
    type Partial = TracePartial;

    fn requirements(&self) -> OutputRequirements {
        self.requirements
    }

    fn merge_order(&self) -> MergeOrder {
        MergeOrder::Scan
    }

    fn open_partial(&self, partition: &PartitionInfo) -> Result<Self::Partial, SinkError> {
        Ok(TracePartial {
            trace: PartitionTrace {
                info: partition.clone(),
                num_batches: 0,
                num_records: 0,
                summary: None,
            },
            requirements: self.requirements,
            num_batches_before_control: self.num_batches_before_control,
            control: self.control,
        })
    }

    fn merge(
        &self,
        output: &mut Self::Output,
        partial: PartitionTrace,
    ) -> Result<SinkControl, SinkError> {
        output.push(partial);
        Ok(SinkControl::Continue)
    }
}

/// The partial sink of [`TraceSink`].
#[derive(Debug)]
struct TracePartial {
    trace: PartitionTrace,
    requirements: OutputRequirements,
    num_batches_before_control: usize,
    control: SinkControl,
}

impl PartialSink for TracePartial {
    type Output = PartitionTrace;

    fn consume(&mut self, batch: &RecordBatch<'_>) -> Result<SinkControl, SinkError> {
        assert_eq!(batch.json().is_some(), self.requirements.is_json_needed);
        assert_eq!(
            batch.timestamps_ns().is_some(),
            self.requirements.is_timestamp_needed
        );
        let PartitionKind::Archive(archive_id) = &self.trace.info.kind else {
            panic!("only archive partitions exist without top-k");
        };
        assert_eq!(batch.archive_id(0), archive_id);
        self.trace.num_batches += 1;
        self.trace.num_records += batch.num_records();
        if self.trace.num_batches >= self.num_batches_before_control {
            return Ok(self.control);
        }
        Ok(SinkControl::Continue)
    }

    fn finish(mut self, summary: &PartitionSummary) -> Result<Self::Output, SinkError> {
        self.trace.summary = Some(summary.clone());
        Ok(self.trace)
    }
}

/// A sink whose partials fail to consume.
#[derive(Debug)]
struct FailingSink;

impl Sink for FailingSink {
    type Output = ();
    type Partial = FailingPartial;

    fn requirements(&self) -> OutputRequirements {
        OutputRequirements::json()
    }

    fn merge_order(&self) -> MergeOrder {
        MergeOrder::Commutative
    }

    fn open_partial(&self, _partition: &PartitionInfo) -> Result<Self::Partial, SinkError> {
        Ok(FailingPartial)
    }

    fn merge(&self, _output: &mut (), _partial: ()) -> Result<SinkControl, SinkError> {
        Ok(SinkControl::Continue)
    }
}

/// The partial sink of [`FailingSink`].
#[derive(Debug)]
struct FailingPartial;

impl PartialSink for FailingPartial {
    type Output = ();

    fn consume(&mut self, _batch: &RecordBatch<'_>) -> Result<SinkControl, SinkError> {
        Err(SinkError::DownstreamClosed)
    }

    fn finish(self, _summary: &PartitionSummary) -> Result<Self::Output, SinkError> {
        Ok(())
    }
}

/// # Type Parameters
///
/// * `SinkType` - The sink the session pushes its records into.
///
/// # Returns
///
/// The report of a session over the fixtures `names` with `sink`, on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`ReaderContextBuilder::build`]'s return values on failure.
/// * Forwards [`ScanRequestBuilder::build`]'s return values on failure.
/// * Forwards [`SearchSessionBuilder::build`]'s return values on failure.
/// * Forwards [`SearchSession::run`]'s return values on failure.
fn run_session<SinkType: Sink>(
    names: &[&str],
    sink: SinkType,
    is_failed_archive_skipped: bool,
) -> Result<SessionReport<SinkType::Output>, ReadError> {
    let context = ReaderContext::builder().build()?;
    let request = Arc::new(ScanRequest::builder().build()?);
    names
        .iter()
        .fold(SearchSession::builder(context, request), |builder, name| {
            builder.archive(
                ArchiveId::from(format!("{name}.clps")),
                ArchiveLocation::File(fixture(name)),
            )
        })
        .skip_failed_archives(is_failed_archive_skipped)
        .build()?
        .run(Arc::new(sink))
}

#[test]
fn partitions_follow_archive_order_with_one_summary_each() -> anyhow::Result<()> {
    let names = ["logs.ts-split.3", "logs.ts-split.0", "many-schemas.tables0"];
    let report = run_session(&names, TraceSink::JSON, false)?;
    assert_eq!(report.output.len(), 3);
    assert_eq!(report.archive_summaries.len(), 3);
    for (index, ((trace, summary), name)) in report
        .output
        .iter()
        .zip(&report.archive_summaries)
        .zip(names)
        .enumerate()
    {
        let archive_id = ArchiveId::from(format!("{name}.clps"));
        assert_eq!(trace.info.index as usize, index);
        assert_eq!(trace.info.kind, PartitionKind::Archive(archive_id.clone()));
        assert_eq!(trace.info.order, ResultOrder::Any);
        assert_eq!(summary.archive_id, archive_id);
        assert_eq!(summary.termination_stage, Some(TerminationStage::ErtScan));
        assert_eq!(summary.error.as_ref().map(ToString::to_string), None);
        assert_eq!(summary.stats.num_records_selected, trace.num_records as u64);
        let partition_summary = trace.summary.as_ref().expect("every partition finishes");
        assert_eq!(partition_summary.stats, summary.stats);
        assert!(0 < trace.num_records, "{name}");
    }
    assert_eq!(report.output[2].num_records, 240);
    Ok(())
}

#[test]
fn sinks_can_stop_an_archive_or_the_session() -> anyhow::Result<()> {
    let names = ["logs.ts", "many-schemas.tables0"];
    let stop_archive = run_session(
        &names,
        TraceSink {
            num_batches_before_control: 1,
            control: SinkControl::StopArchive,
            ..TraceSink::JSON
        },
        false,
    )?;
    assert_eq!(
        stop_archive
            .output
            .iter()
            .map(|trace| trace.num_batches)
            .collect::<Vec<_>>(),
        vec![1, 1]
    );
    assert!(
        stop_archive
            .archive_summaries
            .iter()
            .all(|summary| Some(TerminationStage::StoppedBySink) == summary.termination_stage)
    );

    let stop_session = run_session(
        &names,
        TraceSink {
            num_batches_before_control: 1,
            control: SinkControl::StopSession,
            ..TraceSink::JSON
        },
        false,
    )?;
    assert_eq!(stop_session.output.len(), 1);
    assert_eq!(stop_session.archive_summaries.len(), 2);
    assert_eq!(
        stop_session.archive_summaries[1].termination_stage,
        Some(TerminationStage::StoppedBySink)
    );
    assert_eq!(
        stop_session.archive_summaries[1].stats.num_tables_planned,
        0
    );
    Ok(())
}

#[test]
fn failed_archives_fail_the_session_unless_skipped() -> anyhow::Result<()> {
    let work = tempfile::tempdir()?;
    let corrupt = work.path().join("corrupt.clps");
    std::fs::write(&corrupt, vec![0_u8; 1024])?;
    let context = ReaderContext::builder().build()?;
    let request = Arc::new(ScanRequest::builder().build()?);
    let session = |is_failed_archive_skipped| {
        SearchSession::builder(Arc::clone(&context), Arc::clone(&request))
            .archive(
                ArchiveId::from("corrupt.clps"),
                ArchiveLocation::File(corrupt.clone()),
            )
            .archive(
                ArchiveId::from("logs.ts.clps"),
                ArchiveLocation::File(fixture("logs.ts")),
            )
            .skip_failed_archives(is_failed_archive_skipped)
            .build()
    };

    let error = session(false)?
        .run(Arc::new(TraceSink::JSON))
        .expect_err("the corrupt archive fails the session");
    assert_eq!(error.kind(), ErrorKind::CorruptArchive, "{error}");
    assert!(
        matches!(
            &error,
            ReadError::InArchive { archive_id, .. } if "corrupt.clps" == archive_id.as_str()
        ),
        "{error:?}"
    );

    let report = session(true)?.run(Arc::new(TraceSink::JSON))?;
    assert_eq!(report.archive_summaries.len(), 2);
    let failed = &report.archive_summaries[0];
    assert_eq!(failed.termination_stage, None);
    assert_eq!(
        failed.error.as_ref().map(ReadError::kind),
        Some(ErrorKind::CorruptArchive)
    );
    assert_eq!(report.output[0].num_records, 0);
    assert_eq!(report.output[1].num_records, 3000);
    Ok(())
}

#[test]
fn sink_failures_fail_the_session() {
    let error = run_session(&["logs.ts"], FailingSink, true)
        .expect_err("the sink failure fails the session");
    assert_eq!(error.kind(), ErrorKind::Sink, "{error}");
    assert!(
        matches!(&error, ReadError::Sink(sink) if matches!(**sink, SinkError::DownstreamClosed)),
        "{error:?}"
    );
}

#[test]
fn sink_requirements_are_merged_into_the_request() -> anyhow::Result<()> {
    let report = run_session(
        &["logs.ts"],
        TraceSink {
            requirements: OutputRequirements::none().with_timestamps(),
            ..TraceSink::JSON
        },
        false,
    )?;
    assert_eq!(report.output[0].num_records, 3000);

    let limit = NonZeroU32::new(5).expect("non-zero");
    let conflict = run_session(
        &["logs.ts"],
        TraceSink {
            requirements: OutputRequirements::json()
                .with_required_order(ResultOrder::LatestFirst { limit }),
            ..TraceSink::JSON
        },
        false,
    );
    assert!(
        matches!(conflict, Err(ReadError::InvalidArgument(_))),
        "{conflict:?}"
    );
    Ok(())
}

#[test]
fn duplicate_archive_ids_are_rejected() -> anyhow::Result<()> {
    let context = ReaderContext::builder().build()?;
    let request = Arc::new(ScanRequest::builder().build()?);
    let result = SearchSession::builder(context, request)
        .archive(
            ArchiveId::from("a.clps"),
            ArchiveLocation::File(fixture("logs.ts")),
        )
        .archive(
            ArchiveId::from("a.clps"),
            ArchiveLocation::File(fixture("logs.ts-split.0")),
        )
        .build();
    assert!(
        matches!(result, Err(ReadError::InvalidArgument(_))),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn cancelled_sessions_fail_without_skipping() -> anyhow::Result<()> {
    let context = ReaderContext::builder().build()?;
    let request = Arc::new(ScanRequest::builder().build()?);
    let token = CancellationToken::new();
    token.cancel();
    let result = SearchSession::builder(context, request)
        .archive(
            ArchiveId::from("logs.ts.clps"),
            ArchiveLocation::File(fixture("logs.ts")),
        )
        .cancellation_token(token)
        .skip_failed_archives(true)
        .build()?
        .run(Arc::new(TraceSink::JSON));
    let error = result.expect_err("a cancelled session fails");
    assert_eq!(error.kind(), ErrorKind::Cancelled, "{error}");
    Ok(())
}
