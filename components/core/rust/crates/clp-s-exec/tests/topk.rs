//! Session-wide top-k (`ResultOrder::LatestFirst`) over C++ fixture archives: the winners equal
//! the latest-first prefix of every match, ties and missing timestamps follow the total order,
//! archives that cannot win are skipped unread, and the winners reach the sink once.

use std::num::NonZeroU32;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use clp_s_exec::Archive;
use clp_s_exec::ArchiveId;
use clp_s_exec::ArchiveLocation;
use clp_s_exec::ArchiveSummary;
use clp_s_exec::BatchRows;
use clp_s_exec::LatestFirstKey;
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
use clp_s_query::Query;
use clp_s_query::QueryOptions;
use clp_s_resources::CancellationToken;
use clp_s_search::TerminationStage;
use clp_s_table::RecordId;

/// # Returns
///
/// The path of fixture archive `name`.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/archives")
        .join(format!("{name}.clps"))
}

/// # Returns
///
/// The nine archives of the `logs.ts-split` fixture.
fn split_archives() -> Vec<(String, PathBuf)> {
    (0..9)
        .map(|index| {
            let name = format!("logs.ts-split.{index}");
            (format!("{name}.clps"), fixture(&name))
        })
        .collect()
}

/// # Returns
///
/// The limit `limit`.
const fn limit(limit: u32) -> NonZeroU32 {
    NonZeroU32::new(limit).expect("the limit is non-zero")
}

/// One delivered record.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Delivered {
    archive_id: ArchiveId,
    record_id: RecordId,
    timestamp_ns: Option<i64>,
    log_event_idx: Option<i64>,
    json: Vec<u8>,
}

impl Delivered {
    /// # Returns
    ///
    /// The record's latest-first key.
    fn key(&self) -> LatestFirstKey {
        LatestFirstKey {
            timestamp_ms: self
                .timestamp_ns
                .map(|timestamp| timestamp.div_euclid(1_000_000)),
            archive_id: self.archive_id.clone(),
            record_id: self.record_id,
        }
    }
}

/// What one partition received.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Partition {
    info: PartitionInfo,
    num_batches: usize,
    is_gathered: bool,
    records: Vec<Delivered>,
}

/// Collects every record with everything a wire sink needs, per partition.
#[derive(Debug)]
struct GatherSink;

impl Sink for GatherSink {
    type Output = Vec<Partition>;
    type Partial = GatherPartial;

    fn requirements(&self) -> OutputRequirements {
        OutputRequirements::json()
            .with_timestamps()
            .with_log_event_indices()
    }

    fn merge_order(&self) -> MergeOrder {
        MergeOrder::Scan
    }

    fn open_partial(&self, partition: &PartitionInfo) -> Result<Self::Partial, SinkError> {
        Ok(GatherPartial {
            partition: Partition {
                info: partition.clone(),
                num_batches: 0,
                is_gathered: false,
                records: Vec::new(),
            },
        })
    }

    fn merge(
        &self,
        output: &mut Self::Output,
        partial: Partition,
    ) -> Result<SinkControl, SinkError> {
        output.push(partial);
        Ok(SinkControl::Continue)
    }
}

/// The partial sink of [`GatherSink`].
#[derive(Debug)]
struct GatherPartial {
    partition: Partition,
}

impl PartialSink for GatherPartial {
    type Output = Partition;

    fn consume(&mut self, batch: &RecordBatch<'_>) -> Result<SinkControl, SinkError> {
        self.partition.num_batches += 1;
        self.partition.is_gathered = matches!(batch.rows(), BatchRows::Gathered(_));
        self.partition.records.extend(gather(batch));
        Ok(SinkControl::Continue)
    }

    fn finish(self, _summary: &PartitionSummary) -> Result<Self::Output, SinkError> {
        Ok(self.partition)
    }
}

/// # Returns
///
/// The records of `batch`.
fn gather(batch: &RecordBatch<'_>) -> Vec<Delivered> {
    let json = batch.json().expect("JSON was requested");
    let timestamps_ns = batch.timestamps_ns().expect("timestamps were requested");
    let log_event_indices = batch
        .log_event_indices()
        .expect("log event indices were requested");
    (0..batch.num_records())
        .map(|index| Delivered {
            archive_id: batch.archive_id(index).clone(),
            record_id: batch.record_ids()[index],
            timestamp_ns: timestamps_ns[index],
            log_event_idx: log_event_indices[index],
            json: json
                .record(index)
                .expect("every record is rendered")
                .to_vec(),
        })
        .collect()
}

/// # Returns
///
/// The report of a session running `kql` in `order` over `archives` (id, path), on success.
///
/// # Errors
///
/// Returns an error if the session fails.
fn run(
    archives: &[(String, PathBuf)],
    kql: &str,
    order: ResultOrder,
    is_failed_archive_skipped: bool,
) -> Result<SessionReport<Vec<Partition>>, ReadError> {
    let context = ReaderContext::builder().build()?;
    let request = ScanRequest::builder()
        .query(Query::from_kql(kql, &QueryOptions::default())?)
        .order(order)
        .build()?;
    archives
        .iter()
        .fold(
            SearchSession::builder(context, Arc::new(request)),
            |builder, (archive_id, path)| {
                builder.archive(
                    ArchiveId::from(archive_id.as_str()),
                    ArchiveLocation::File(path.clone()),
                )
            },
        )
        .skip_failed_archives(is_failed_archive_skipped)
        .build()?
        .run(Arc::new(GatherSink))
}

/// # Returns
///
/// The first `limit` matches of `kql` over `archives` in latest-first order, computed from a
/// scan-order session, on success.
///
/// # Errors
///
/// Returns an error if the session fails.
fn latest_first_prefix(
    archives: &[(String, PathBuf)],
    kql: &str,
    limit: usize,
) -> Result<Vec<Delivered>, ReadError> {
    let report = run(archives, kql, ResultOrder::Scan, false)?;
    let mut records: Vec<Delivered> = report
        .output
        .into_iter()
        .flat_map(|partition| partition.records)
        .collect();
    records.sort_by_key(Delivered::key);
    records.truncate(limit);
    Ok(records)
}

/// Asserts the partition shape of a top-k session over `num_archives` archives, and returns the
/// winners.
///
/// # Returns
///
/// The records of the top-k partition.
fn top_k_winners(report: &SessionReport<Vec<Partition>>, num_archives: usize) -> Vec<Delivered> {
    let partitions = &report.output;
    assert_eq!(partitions.len(), num_archives + 1, "{partitions:?}");
    for (index, partition) in partitions.iter().enumerate() {
        assert_eq!(partition.info.index as usize, index);
    }
    let (top_k, archives) = partitions.split_last().expect("a top-k partition exists");
    for archive in archives {
        assert!(
            matches!(archive.info.kind, PartitionKind::Archive(_)),
            "{archive:?}"
        );
        assert_eq!(archive.num_batches, 0, "{:?}", archive.info);
    }
    assert_eq!(top_k.info.kind, PartitionKind::TopK);
    if top_k.records.is_empty() {
        assert_eq!(top_k.num_batches, 0);
    } else {
        assert_eq!(top_k.num_batches, 1);
        assert!(top_k.is_gathered);
    }
    top_k.records.clone()
}

/// # Returns
///
/// The summaries of `report` whose archive was skipped unread by the top-k.
fn skipped(report: &SessionReport<Vec<Partition>>) -> Vec<&ArchiveSummary> {
    report
        .archive_summaries
        .iter()
        .filter(|summary| summary.termination_stage == Some(TerminationStage::TimeRangeMatching))
        .collect()
}

#[test]
fn winners_are_the_latest_first_prefix_of_every_match() -> anyhow::Result<()> {
    let archives = split_archives();
    for kql in ["*", "level: ERROR", "level: ERROR AND service: auth"] {
        for k in [1, 2, 10, 37, 2999, 3000, 5000] {
            let report = run(
                &archives,
                kql,
                ResultOrder::LatestFirst { limit: limit(k) },
                false,
            )?;
            let winners = top_k_winners(&report, archives.len());
            let expected = latest_first_prefix(&archives, kql, k as usize)?;
            assert!(!expected.is_empty(), "{kql}");
            assert_eq!(winners, expected, "{kql}, k = {k}");
        }
    }
    Ok(())
}

#[test]
fn timestamp_ties_break_by_archive_id_then_record_id() -> anyhow::Result<()> {
    let archives = vec![
        ("b.clps".to_owned(), fixture("logs.ts")),
        ("a.clps".to_owned(), fixture("logs.ts")),
    ];
    let report = run(
        &archives,
        "*",
        ResultOrder::LatestFirst { limit: limit(7) },
        false,
    )?;
    let winners = top_k_winners(&report, 2);
    let names: Vec<(&str, i64)> = winners
        .iter()
        .map(|record| (record.archive_id.as_str(), record.record_id.get()))
        .collect();
    assert_eq!(
        names,
        vec![
            ("a.clps", 2999),
            ("b.clps", 2999),
            ("a.clps", 2998),
            ("b.clps", 2998),
            ("a.clps", 2997),
            ("b.clps", 2997),
            ("a.clps", 2996),
        ]
    );
    assert_eq!(winners, latest_first_prefix(&archives, "*", 7)?);
    Ok(())
}

#[test]
fn records_without_a_timestamp_rank_after_every_timestamp() -> anyhow::Result<()> {
    let archives = vec![
        ("0-untimed.clps".to_owned(), fixture("search.default")),
        ("z-timed.clps".to_owned(), fixture("search.ts")),
    ];
    let report = run(
        &archives,
        "*",
        ResultOrder::LatestFirst { limit: limit(45) },
        false,
    )?;
    let winners = top_k_winners(&report, 2);
    assert_eq!(winners, latest_first_prefix(&archives, "*", 45)?);
    let num_timed = winners
        .iter()
        .take_while(|record| record.timestamp_ns.is_some())
        .count();
    assert!(num_timed > 30, "{winners:?}");
    assert!(
        winners[..num_timed]
            .iter()
            .all(|record| record.archive_id.as_str() == "z-timed.clps"),
        "{winners:?}"
    );
    let untimed: Vec<(&str, i64)> = winners[num_timed..]
        .iter()
        .map(|record| {
            assert_eq!(record.timestamp_ns, None, "{record:?}");
            (record.archive_id.as_str(), record.record_id.get())
        })
        .collect();
    let expected_untimed: Vec<(&str, i64)> = (0_i64..)
        .take(45 - num_timed)
        .map(|record_id| ("0-untimed.clps", record_id))
        .collect();
    assert_eq!(untimed, expected_untimed);
    Ok(())
}

#[test]
fn negative_and_missing_timestamps_order_by_their_floored_milliseconds() -> anyhow::Result<()> {
    let archives = vec![(
        "timestamps-int.ts.clps".to_owned(),
        fixture("timestamps-int.ts"),
    )];
    let report = run(
        &archives,
        "*",
        ResultOrder::LatestFirst { limit: limit(100) },
        false,
    )?;
    let winners = top_k_winners(&report, 1);
    let expected = latest_first_prefix(&archives, "*", 100)?;
    assert_eq!(winners, expected);
    let keys: Vec<Option<i64>> = winners
        .iter()
        .map(|record| record.key().timestamp_ms)
        .collect();
    assert!(keys.contains(&None), "{keys:?}");
    assert!(
        keys.iter().flatten().any(|timestamp| *timestamp < 0),
        "{keys:?}"
    );
    Ok(())
}

#[test]
fn archives_that_cannot_win_are_skipped_unread() -> anyhow::Result<()> {
    let archives = split_archives();
    let report = run(
        &archives,
        "*",
        ResultOrder::LatestFirst { limit: limit(5) },
        false,
    )?;
    assert_eq!(
        top_k_winners(&report, archives.len()),
        latest_first_prefix(&archives, "*", 5)?
    );
    let skipped_summaries = skipped(&report);
    assert!(
        skipped_summaries.len() >= archives.len() - 2,
        "{:?}",
        report.archive_summaries
    );
    for summary in &skipped_summaries {
        assert_eq!(summary.stats.num_streams_decoded, 0, "{summary:?}");
        assert_eq!(summary.stats.num_bytes_read, 0, "{summary:?}");
    }
    let ids: Vec<&str> = report
        .archive_summaries
        .iter()
        .map(|summary| summary.archive_id.as_str())
        .collect();
    let expected_ids: Vec<&str> = archives.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(
        ids, expected_ids,
        "summaries keep the order of the archives"
    );

    let unlimited = run(
        &archives,
        "*",
        ResultOrder::LatestFirst {
            limit: limit(10_000),
        },
        false,
    )?;
    let skipped_archives = skipped(&unlimited);
    assert!(skipped_archives.is_empty(), "{skipped_archives:?}");
    Ok(())
}

#[test]
fn sessions_with_many_archives_reopen_them_and_keep_the_same_winners() -> anyhow::Result<()> {
    let archives: Vec<(String, PathBuf)> = (0..8)
        .flat_map(|copy| {
            split_archives()
                .into_iter()
                .map(move |(archive_id, path)| (format!("{copy}-{archive_id}"), path))
        })
        .collect();
    assert!(archives.len() > 64, "{}", archives.len());
    for (kql, k) in [("*", 25), ("level: ERROR", 37), ("level: ERROR", 2000)] {
        let report = run(
            &archives,
            kql,
            ResultOrder::LatestFirst { limit: limit(k) },
            false,
        )?;
        assert_eq!(
            top_k_winners(&report, archives.len()),
            latest_first_prefix(&archives, kql, k as usize)?,
            "{kql}, k = {k}"
        );
    }
    Ok(())
}

#[test]
fn only_records_the_top_k_keeps_are_rendered() -> anyhow::Result<()> {
    let k = 7;
    let archives = vec![
        ("a.clps".to_owned(), fixture("logs.ts")),
        ("b.clps".to_owned(), fixture("logs.ts")),
    ];
    let report = run(
        &archives,
        "*",
        ResultOrder::LatestFirst { limit: limit(k) },
        false,
    )?;
    assert_eq!(
        top_k_winners(&report, archives.len()),
        latest_first_prefix(&archives, "*", k as usize)?
    );
    let second = &report.archive_summaries[1];
    assert_eq!(second.archive_id.as_str(), "b.clps");
    assert_eq!(second.stats.num_records_selected, 3000, "{second:?}");
    assert!(
        second.stats.num_records_rendered <= u64::from(k),
        "the second archive renders only the records that beat the first one's: {second:?}"
    );

    let scan_order = run(&archives, "level: ERROR", ResultOrder::Scan, false)?;
    for summary in &scan_order.archive_summaries {
        assert!(0 < summary.stats.num_records_selected, "{summary:?}");
        assert_eq!(
            summary.stats.num_records_rendered, summary.stats.num_records_selected,
            "{summary:?}"
        );
    }
    Ok(())
}

#[test]
fn archives_without_timestamps_are_skipped_once_the_top_k_is_timed() -> anyhow::Result<()> {
    let archives = vec![
        ("untimed.clps".to_owned(), fixture("search.default")),
        ("timed.clps".to_owned(), fixture("search.ts")),
    ];
    let report = run(
        &archives,
        "*",
        ResultOrder::LatestFirst { limit: limit(3) },
        false,
    )?;
    assert_eq!(
        top_k_winners(&report, 2),
        latest_first_prefix(&archives, "*", 3)?
    );
    let skipped_ids: Vec<&str> = skipped(&report)
        .iter()
        .map(|summary| summary.archive_id.as_str())
        .collect();
    assert_eq!(skipped_ids, vec!["untimed.clps"]);
    assert_eq!(
        report.output[0].info.kind,
        PartitionKind::Archive(ArchiveId::from("timed.clps")),
        "the timed archive is searched first"
    );
    Ok(())
}

#[test]
fn failed_archives_are_skipped_or_fail_the_top_k_session() -> anyhow::Result<()> {
    let work = tempfile::tempdir()?;
    let empty = work.path().join("empty.clps");
    std::fs::write(&empty, b"")?;
    let mut archives = split_archives();
    archives.insert(3, ("empty.clps".to_owned(), empty));
    let order = ResultOrder::LatestFirst { limit: limit(20) };
    let report = run(&archives, "level: ERROR", order, true)?;
    assert_eq!(
        top_k_winners(&report, archives.len()),
        latest_first_prefix(&split_archives(), "level: ERROR", 20)?
    );
    let failed: Vec<&str> = report
        .archive_summaries
        .iter()
        .filter(|summary| summary.error.is_some())
        .map(|summary| summary.archive_id.as_str())
        .collect();
    assert_eq!(failed, vec!["empty.clps"]);

    let error = run(&archives, "level: ERROR", order, false)
        .expect_err("a failed archive fails the session");
    let ReadError::InArchive { archive_id, .. } = &error else {
        anyhow::bail!("expected an archive error, got {error:?}");
    };
    assert_eq!(archive_id.as_str(), "empty.clps");
    Ok(())
}

#[test]
fn queries_without_matches_deliver_no_batch() -> anyhow::Result<()> {
    let archives = split_archives();
    for kql in ["NOT *", "level: NO_SUCH_LEVEL"] {
        let report = run(
            &archives,
            kql,
            ResultOrder::LatestFirst { limit: limit(3) },
            false,
        )?;
        assert_eq!(top_k_winners(&report, archives.len()), Vec::new(), "{kql}");
    }
    Ok(())
}

#[test]
fn the_pull_cursor_delivers_the_archive_top_k_once() -> anyhow::Result<()> {
    let context = ReaderContext::builder().build()?;
    let archive = Archive::open(
        context,
        ArchiveId::from("logs.ts.clps"),
        ArchiveLocation::File(fixture("logs.ts")),
    )?;
    let request = ScanRequest::builder()
        .query(Query::from_kql("level: WARN", &QueryOptions::default())?)
        .outputs(
            OutputRequirements::json()
                .with_timestamps()
                .with_log_event_indices(),
        )
        .order(ResultOrder::LatestFirst { limit: limit(6) })
        .build()?;
    let mut scan = archive.scan(Arc::new(request), CancellationToken::new())?;
    let batch = scan.next_batch()?.expect("the top-k is delivered");
    assert!(matches!(batch.rows(), BatchRows::Gathered(_)));
    let records = gather(&batch);
    let expected = latest_first_prefix(
        &[("logs.ts.clps".to_owned(), fixture("logs.ts"))],
        "level: WARN",
        6,
    )?;
    assert_eq!(records, expected);
    assert_eq!(
        scan.next_batch()?.as_ref().map(RecordBatch::num_records),
        None
    );
    assert_eq!(
        scan.next_batch()?.as_ref().map(RecordBatch::num_records),
        None
    );
    Ok(())
}
