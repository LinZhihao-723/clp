//! The pull cursor over C++ fixture archives: opening from files and memory, rejections, full
//! scans compared with the C++ `x` oracles, batch splitting, per-record outputs, statistics,
//! cancellation and the memory budget.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use clp_s_exec::Archive;
use clp_s_exec::ArchiveId;
use clp_s_exec::ArchiveLocation;
use clp_s_exec::BatchRows;
use clp_s_exec::ExecutionConfig;
use clp_s_exec::OutputRequirements;
use clp_s_exec::ReadError;
use clp_s_exec::ReaderContext;
use clp_s_exec::ResultOrder;
use clp_s_exec::Scan;
use clp_s_exec::ScanRequest;
use clp_s_exec::TimestampRangeMs;
use clp_s_format::SharedBytes;
use clp_s_io::StorageError;
use clp_s_query::NormalExpr;
use clp_s_query::Query;
use clp_s_query::QueryOptions;
use clp_s_query::TimeBounds;
use clp_s_resources::CancellationToken;
use clp_s_resources::ErrorKind;
use clp_s_resources::ResourceError;

/// # Returns
///
/// The path of fixture archive `name` (e.g. `"logs.ts"`).
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/archives")
        .join(format!("{name}.clps"))
}

/// # Returns
///
/// The path of rejection fixture `name`.
fn reject_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/archives/reject")
        .join(name)
}

/// # Returns
///
/// The C++ `x` oracle lines of fixture `name`, sorted.
fn oracle_lines(name: &str) -> Vec<Vec<u8>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/oracles")
        .join(format!("{name}.jsonl"));
    sorted_lines(&std::fs::read(&path).expect("the oracle is readable"))
}

/// # Returns
///
/// The non-empty lines of `jsonl`, sorted.
fn sorted_lines(jsonl: &[u8]) -> Vec<Vec<u8>> {
    let mut lines: Vec<Vec<u8>> = jsonl
        .split(|byte| b'\n' == *byte)
        .filter(|line| !line.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    lines.sort();
    lines
}

/// # Type Parameters
///
/// * `UpdateType` - Changes the default configuration.
///
/// # Returns
///
/// The context with the default configuration changed by `update`.
fn context_with<UpdateType: FnOnce(&mut ExecutionConfig)>(
    update: UpdateType,
) -> Arc<ReaderContext> {
    let mut config = ExecutionConfig::default();
    update(&mut config);
    ReaderContext::builder()
        .config(config)
        .build()
        .expect("the configuration is valid")
}

/// # Returns
///
/// The archive of fixture `name`, opened from its file with `context`.
fn open(context: &Arc<ReaderContext>, name: &str) -> Archive {
    Archive::open(
        Arc::clone(context),
        ArchiveId::from(format!("{name}.clps")),
        ArchiveLocation::File(fixture(name)),
    )
    .expect("the fixture opens")
}

/// # Returns
///
/// A request with `outputs` and otherwise defaults.
fn request(outputs: OutputRequirements) -> Arc<ScanRequest> {
    Arc::new(
        ScanRequest::builder()
            .outputs(outputs)
            .build()
            .expect("the request is valid"),
    )
}

/// One delivered batch, copied out of the cursor.
#[derive(Debug, Default)]
struct Delivered {
    record_ids: Vec<i64>,
    timestamps_ns: Option<Vec<Option<i64>>>,
    log_event_indices: Option<Vec<Option<i64>>>,
    jsonl: Option<Vec<u8>>,
}

/// # Returns
///
/// Every batch of `scan`, copied, on success.
///
/// # Errors
///
/// Returns an error if the scan fails.
fn drain(scan: &mut Scan) -> Result<Vec<Delivered>, ReadError> {
    let mut batches = Vec::new();
    while let Some(batch) = scan.next_batch()? {
        assert!(0 < batch.num_records(), "an empty batch was delivered");
        if let Some(json) = batch.json() {
            assert_eq!(json.len(), batch.num_records());
        }
        assert_eq!(
            batch.archive_id(batch.num_records() - 1),
            batch.archive_id(0)
        );
        batches.push(Delivered {
            record_ids: batch.record_ids().iter().map(|id| id.get()).collect(),
            timestamps_ns: batch.timestamps_ns().map(<[Option<i64>]>::to_vec),
            log_event_indices: batch.log_event_indices().map(<[Option<i64>]>::to_vec),
            jsonl: batch.json().map(|json| json.as_jsonl().to_vec()),
        });
    }
    Ok(batches)
}

/// # Returns
///
/// The timestamp of every record of `batches`, by record id.
fn timestamps_by_record_id(batches: &[Delivered]) -> HashMap<i64, Option<i64>> {
    batches
        .iter()
        .flat_map(|batch| {
            batch
                .record_ids
                .iter()
                .copied()
                .zip(batch.timestamps_ns.clone().expect("timestamps requested"))
        })
        .collect()
}

/// # Returns
///
/// The concatenated JSON lines of `batches`.
fn concat_jsonl(batches: &[Delivered]) -> Vec<u8> {
    batches
        .iter()
        .flat_map(|batch| batch.jsonl.clone().expect("JSON was requested"))
        .collect()
}

#[test]
fn archives_open_from_files_and_memory_with_their_info() -> anyhow::Result<()> {
    let context = ReaderContext::builder().build()?;
    let path = fixture("logs.ts");
    let from_file = open(&context, "logs.ts");
    let bytes = SharedBytes::from(std::fs::read(&path)?);
    let from_memory = Archive::open(
        Arc::clone(&context),
        ArchiveId::from("logs.ts.clps"),
        ArchiveLocation::Memory(bytes.clone()),
    )?;
    assert_eq!(from_file.info(), from_memory.info());
    let info = from_file.info();
    assert_eq!(info.archive_id.as_str(), "logs.ts.clps");
    assert_eq!(
        (info.version.major, info.version.minor, info.version.patch),
        (0, 5, 1)
    );
    assert_eq!(info.num_log_events, 3000);
    assert!(info.has_log_order);
    assert_eq!(info.file_size, bytes.len() as u64);
    let Some(TimestampRangeMs { begin, end }) = info.timestamp_range_ms else {
        anyhow::bail!("logs.ts has a timestamp range: {info:?}");
    };
    assert!(begin <= end, "{begin} > {end}");
    assert!(!from_file.tables().is_empty());

    let nolog = open(&context, "scalars.nolog");
    assert!(!nolog.info().has_log_order);
    assert_eq!(nolog.info().timestamp_range_ms, None);

    let request = request(OutputRequirements::json());
    let file_jsonl = concat_jsonl(&drain(
        &mut from_file.scan(Arc::clone(&request), CancellationToken::new())?,
    )?);
    let memory_jsonl = concat_jsonl(&drain(
        &mut from_memory.scan(request, CancellationToken::new())?,
    )?);
    assert_eq!(file_jsonl, memory_jsonl);
    Ok(())
}

#[test]
fn unsupported_and_broken_archives_fail_at_open() -> anyhow::Result<()> {
    let context = ReaderContext::builder().build()?;
    let open_location = |location| {
        Archive::open(Arc::clone(&context), ArchiveId::from("archive"), location)
            .expect_err("the archive must not open")
    };

    let multi_file = open_location(ArchiveLocation::File(reject_fixture("scalars.mfa")));
    assert_eq!(multi_file.kind(), ErrorKind::UnsupportedArchive);
    assert!(
        matches!(&multi_file, ReadError::Storage(error)
            if matches!(**error, StorageError::MultiFileArchive { .. })),
        "{multi_file:?}"
    );
    let structured = open_location(ArchiveLocation::File(reject_fixture(
        "arrays.structurize-arrays.clps",
    )));
    assert_eq!(
        structured.kind(),
        ErrorKind::UnsupportedArchive,
        "{structured}"
    );

    let work = tempfile::tempdir()?;
    let missing = open_location(ArchiveLocation::File(work.path().join("missing.clps")));
    assert_eq!(missing.kind(), ErrorKind::Io, "{missing}");

    let bytes = std::fs::read(fixture("logs.ts"))?;
    let truncated = open_location(ArchiveLocation::Memory(SharedBytes::from(
        bytes[..bytes.len() / 2].to_vec(),
    )));
    assert_eq!(truncated.kind(), ErrorKind::CorruptArchive, "{truncated}");
    let garbage = open_location(ArchiveLocation::Memory(SharedBytes::from(vec![0x5a; 4096])));
    assert_eq!(garbage.kind(), ErrorKind::CorruptArchive, "{garbage}");
    Ok(())
}

#[test]
fn full_scans_reproduce_the_cpp_oracles() -> anyhow::Result<()> {
    let context = ReaderContext::builder().build()?;
    let request = request(OutputRequirements::json());
    for name in [
        "logs.ts",
        "logs.ts-tables4k",
        "many-schemas.tables0",
        "many-schemas.tables256",
        "many-schemas.nolog",
        "strings.default",
        "keys.default",
        "nested.default",
        "arrays.default",
        "polymorphic.default",
        "timestamps-string.ts",
        "long-strings.default",
        "kvir-text.ts",
    ] {
        let archive = open(&context, name);
        let mut scan = archive.scan(Arc::clone(&request), CancellationToken::new())?;
        let jsonl = concat_jsonl(&drain(&mut scan)?);
        assert_eq!(sorted_lines(&jsonl), oracle_lines(name), "{name}");
        let stats = scan.stats();
        let num_tables = archive.tables().len() as u64;
        assert_eq!(stats.num_tables_planned, num_tables, "{name}");
        assert_eq!(stats.num_tables_scanned, num_tables, "{name}");
        assert_eq!(
            stats.num_rows_scanned,
            archive.info().num_log_events,
            "{name}"
        );
        assert_eq!(
            stats.num_records_selected,
            archive.info().num_log_events,
            "{name}"
        );
        assert!(0 < stats.num_streams_decoded, "{name}: {stats:?}");
        assert!(0 < stats.num_decoded_bytes, "{name}: {stats:?}");
        assert!(0 < stats.num_bytes_read, "{name}: {stats:?}");
        assert_eq!(scan.next_batch()?.map(|batch| batch.num_records()), None);
    }
    Ok(())
}

#[test]
fn windows_split_into_batches_by_rendered_size() -> anyhow::Result<()> {
    let request = request(OutputRequirements::json());
    let reference = concat_jsonl(&drain(
        &mut open(&ReaderContext::builder().build()?, "logs.ts")
            .scan(Arc::clone(&request), CancellationToken::new())?,
    )?);

    let one_record = context_with(|config| config.max_rendered_batch_size = 1);
    let batches = drain(
        &mut open(&one_record, "logs.ts").scan(Arc::clone(&request), CancellationToken::new())?,
    )?;
    assert_eq!(batches.len(), 3000);
    assert!(batches.iter().all(|batch| 1 == batch.record_ids.len()));
    assert_eq!(concat_jsonl(&batches), reference);

    let max_rendered_batch_size = 4096;
    let small = context_with(|config| {
        config.max_rendered_batch_size = max_rendered_batch_size;
        config.batch_num_rows = NonZeroU32::new(1000).expect("non-zero");
    });
    let batches =
        drain(&mut open(&small, "logs.ts").scan(Arc::clone(&request), CancellationToken::new())?)?;
    let max_record_size = sorted_lines(&reference)
        .iter()
        .map(|line| line.len() as u64 + 1)
        .max()
        .expect("there are records");
    for batch in &batches {
        let size = batch.jsonl.as_ref().map_or(0, Vec::len) as u64;
        assert!(
            size < max_rendered_batch_size + max_record_size,
            "a batch of {size} bytes"
        );
    }
    assert!(batches.len() as u64 > 3000 * 100 / max_rendered_batch_size);
    assert_eq!(concat_jsonl(&batches), reference);

    let tiny_windows =
        context_with(|config| config.batch_num_rows = NonZeroU32::new(7).expect("non-zero"));
    let batches =
        drain(&mut open(&tiny_windows, "logs.ts").scan(request, CancellationToken::new())?)?;
    assert!(batches.iter().all(|batch| batch.record_ids.len() <= 7));
    assert_eq!(concat_jsonl(&batches), reference);
    Ok(())
}

#[test]
fn per_record_outputs_are_delivered_only_when_requested() -> anyhow::Result<()> {
    let context = ReaderContext::builder().build()?;
    let archive = open(&context, "logs.ts");

    let ids_only = drain(&mut archive.scan(
        request(OutputRequirements::none()),
        CancellationToken::new(),
    )?)?;
    assert!(ids_only.iter().all(|batch| batch.jsonl.is_none()
        && batch.timestamps_ns.is_none()
        && batch.log_event_indices.is_none()));
    let mut record_ids: Vec<i64> = ids_only
        .iter()
        .flat_map(|batch| batch.record_ids.clone())
        .collect();
    record_ids.sort_unstable();
    assert_eq!(record_ids, (0..3000).collect::<Vec<_>>());

    let everything = drain(
        &mut archive.scan(
            request(
                OutputRequirements::json()
                    .with_timestamps()
                    .with_log_event_indices(),
            ),
            CancellationToken::new(),
        )?,
    )?;
    for batch in &everything {
        let timestamps = batch.timestamps_ns.as_ref().expect("timestamps requested");
        let indices = batch.log_event_indices.as_ref().expect("indices requested");
        let jsonl = batch.jsonl.as_ref().expect("JSON requested");
        let lines: Vec<&[u8]> = jsonl
            .split(|byte| b'\n' == *byte)
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(lines.len(), batch.record_ids.len());
        for (row, line) in lines.iter().enumerate() {
            let record: serde_json::Value = serde_json::from_slice(line)?;
            let ts = record["ts"]
                .as_i64()
                .expect("logs records have an integer ts");
            let seq = record["seq"].as_i64().expect("logs records have a seq");
            assert_eq!(timestamps[row], Some(ts * 1_000_000), "{record}");
            assert_eq!(indices[row], Some(seq), "{record}");
            assert_eq!(batch.record_ids[row], seq, "{record}");
        }
    }

    let timestamps_only = drain(&mut archive.scan(
        request(OutputRequirements::none().with_timestamps()),
        CancellationToken::new(),
    )?)?;
    assert_eq!(
        timestamps_by_record_id(&timestamps_only),
        timestamps_by_record_id(&everything)
    );
    Ok(())
}

#[test]
fn archives_without_log_order_or_timestamps_deliver_none() -> anyhow::Result<()> {
    let context = ReaderContext::builder().build()?;
    let nolog = drain(
        &mut open(&context, "scalars.nolog").scan(
            request(
                OutputRequirements::none()
                    .with_timestamps()
                    .with_log_event_indices(),
            ),
            CancellationToken::new(),
        )?,
    )?;
    let mut record_ids = Vec::new();
    for batch in &nolog {
        assert!(
            batch
                .log_event_indices
                .as_ref()
                .is_some_and(|indices| indices.iter().all(Option::is_none)),
            "{batch:?}"
        );
        assert!(
            batch
                .timestamps_ns
                .as_ref()
                .is_some_and(|timestamps| timestamps.iter().all(Option::is_none)),
            "{batch:?}"
        );
        record_ids.extend(batch.record_ids.iter().copied());
    }
    record_ids.sort_unstable();
    assert_eq!(record_ids, (0..11).collect::<Vec<_>>());
    Ok(())
}

#[test]
fn split_archives_number_their_records_by_log_order() -> anyhow::Result<()> {
    let context = ReaderContext::builder().build()?;
    let mut by_record_id: HashMap<i64, usize> = HashMap::new();
    for part in 0..9 {
        let archive = open(&context, &format!("logs.ts-split.{part}"));
        for batch in drain(&mut archive.scan(
            request(OutputRequirements::none().with_log_event_indices()),
            CancellationToken::new(),
        )?)? {
            let indices = batch.log_event_indices.expect("indices requested");
            for (record_id, index) in batch.record_ids.iter().zip(indices) {
                assert_eq!(Some(*record_id), index);
                *by_record_id.entry(*record_id).or_default() += 1;
            }
        }
    }
    assert_eq!(by_record_id.values().sum::<usize>(), 3000);
    Ok(())
}

#[test]
fn queries_that_fold_to_false_are_pruned() -> anyhow::Result<()> {
    let context = ReaderContext::builder().build()?;
    let query = Query::from_kql("NOT *", &QueryOptions::default())?;
    assert_eq!(*query.root(), NormalExpr::MatchNone);
    let request = Arc::new(
        ScanRequest::builder()
            .query(query)
            .outputs(OutputRequirements::json())
            .build()?,
    );
    let mut scan = open(&context, "logs.ts").scan(request, CancellationToken::new())?;
    assert_eq!(scan.next_batch()?.map(|batch| batch.num_records()), None);
    assert_eq!(scan.stats().num_tables_planned, 0);
    assert_eq!(scan.stats().num_streams_decoded, 0);
    Ok(())
}

#[test]
fn cancellation_stops_the_scan() -> anyhow::Result<()> {
    let context =
        context_with(|config| config.batch_num_rows = NonZeroU32::new(100).expect("non-zero"));
    let archive = open(&context, "logs.ts");
    let token = CancellationToken::new();
    let mut scan = archive.scan(request(OutputRequirements::json()), token.clone())?;
    assert!(scan.advance()?);
    scan.cancellation_token().cancel();
    assert!(token.is_cancelled());
    let error = scan.advance().expect_err("a cancelled scan fails");
    assert_eq!(error.kind(), ErrorKind::Cancelled, "{error}");
    assert!(
        matches!(
            &error,
            ReadError::Resource(resource) if matches!(**resource, ResourceError::Cancelled)
        ),
        "{error:?}"
    );
    assert!(!scan.advance()?);
    assert_eq!(scan.current().map(|batch| batch.num_records()), None);

    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let mut scan = archive.scan(request(OutputRequirements::json()), cancelled)?;
    let error = scan.advance().expect_err("a cancelled scan fails");
    assert_eq!(error.kind(), ErrorKind::Cancelled, "{error}");
    Ok(())
}

#[test]
fn the_memory_budget_bounds_stream_decoding() -> anyhow::Result<()> {
    let context = ReaderContext::builder()
        .memory_budget_size(Some(1024))
        .build()?;
    let archive = open(&context, "logs.ts");
    let mut scan = archive.scan(
        request(OutputRequirements::json()),
        CancellationToken::new(),
    )?;
    let error = scan.advance().expect_err("the stream exceeds the budget");
    assert_eq!(error.kind(), ErrorKind::ResourceExhausted, "{error}");
    assert_eq!(context.memory_budget().reserved_size(), 0);
    Ok(())
}

#[test]
fn streams_are_released_before_the_next_is_decoded() -> anyhow::Result<()> {
    for name in ["logs.ts-tables4k", "many-schemas.tables256"] {
        let tables = open(&ReaderContext::builder().build()?, name)
            .tables()
            .to_vec();
        let mut stream_sizes = HashMap::new();
        for info in &tables {
            let size = stream_sizes.entry(info.stream_id).or_insert(0);
            *size = (*size).max(info.stream_offset + info.uncompressed_size);
        }
        assert!(stream_sizes.len() > 1, "{name} has one stream");
        let largest_stream_size = *stream_sizes.values().max().expect("there are streams");

        let context = ReaderContext::builder()
            .memory_budget_size(Some(largest_stream_size))
            .build()?;
        let archive = open(&context, name);
        let mut scan = archive.scan(
            request(OutputRequirements::json()),
            CancellationToken::new(),
        )?;
        let jsonl = concat_jsonl(&drain(&mut scan)?);
        assert_eq!(sorted_lines(&jsonl), oracle_lines(name), "{name}");
        assert_eq!(
            scan.stats().num_streams_decoded,
            stream_sizes.len() as u64,
            "{name}"
        );
    }
    Ok(())
}

#[test]
fn batch_rows_follow_the_record_ordinals() -> anyhow::Result<()> {
    let context = context_with(|config| config.max_rendered_batch_size = 1);
    let archive = open(&context, "logs.ts");
    let mut scan = archive.scan(
        request(OutputRequirements::json()),
        CancellationToken::new(),
    )?;
    let mut num_records = 0;
    while let Some(batch) = scan.next_batch()? {
        let BatchRows::Table {
            batch: rows,
            rows: window_rows,
        } = batch.rows()
        else {
            anyhow::bail!("a scan delivers table rows: {:?}", batch.rows());
        };
        assert_eq!(window_rows.len(), batch.num_records());
        for (window_row, record_id) in window_rows.iter().zip(batch.record_ids()) {
            assert!(rows.selection().is_selected(*window_row));
            assert_eq!(rows.record_id(*window_row)?, *record_id);
        }
        num_records += batch.num_records();
    }
    assert_eq!(num_records, 3000);
    Ok(())
}

#[test]
fn reservations_are_released_after_a_scan() -> anyhow::Result<()> {
    let context = ReaderContext::builder()
        .memory_budget_size(Some(64 * 1024 * 1024))
        .build()?;
    let archive = open(&context, "many-schemas.tables256");
    let mut scan = archive.scan(
        request(OutputRequirements::json()),
        CancellationToken::new(),
    )?;
    drain(&mut scan)?;
    drop(scan);
    assert_eq!(context.memory_budget().reserved_size(), 0);
    Ok(())
}

#[test]
fn requests_validate_and_fold_their_time_bounds() -> anyhow::Result<()> {
    let inverted = ScanRequest::builder()
        .time_bounds(TimeBounds::from_epoch_millis(Some(10), Some(5)))
        .build();
    assert!(
        matches!(inverted, Err(ReadError::InvalidArgument(_))),
        "{inverted:?}"
    );
    let bounds = TimeBounds::from_epoch_millis(Some(5), None);
    let bounded = ScanRequest::builder().time_bounds(bounds).build()?;
    assert_eq!(bounded.time_bounds(), bounds);
    assert_ne!(*bounded.query().root(), NormalExpr::MatchAll);
    let unbounded = ScanRequest::builder().build()?;
    assert_eq!(*unbounded.query().root(), NormalExpr::MatchAll);
    assert_eq!(unbounded.order(), ResultOrder::Any);
    assert_eq!(unbounded.outputs(), OutputRequirements::none());
    Ok(())
}

#[test]
fn output_requirements_merge_and_reject_conflicting_orders() -> anyhow::Result<()> {
    let merged = OutputRequirements::json().merge(OutputRequirements::none().with_timestamps())?;
    assert_eq!(merged, OutputRequirements::json().with_timestamps());
    let limit = NonZeroU32::new(3).expect("non-zero");
    let scan = OutputRequirements::none().with_required_order(ResultOrder::Scan);
    assert_eq!(
        scan.merge(OutputRequirements::json())?.required_order,
        Some(ResultOrder::Scan)
    );
    let conflict = scan
        .merge(OutputRequirements::none().with_required_order(ResultOrder::LatestFirst { limit }));
    assert!(
        matches!(conflict, Err(ReadError::InvalidArgument(_))),
        "{conflict:?}"
    );
    Ok(())
}
