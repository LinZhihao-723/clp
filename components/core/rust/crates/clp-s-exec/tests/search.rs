//! Filtered scans over C++ fixture archives: results compared with the C++ `clp-s s` oracles of the
//! query suite under small windows and batch sizes, projections, time bounds, case-insensitive
//! matching, metadata and dictionary pruning, and sessions over split archives.

use std::num::NonZeroU32;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use clp_s_exec::Archive;
use clp_s_exec::ArchiveId;
use clp_s_exec::ArchiveLocation;
use clp_s_exec::BatchRows;
use clp_s_exec::ExecutionConfig;
use clp_s_exec::MergeOrder;
use clp_s_exec::OutputRequirements;
use clp_s_exec::PartialSink;
use clp_s_exec::PartitionInfo;
use clp_s_exec::PartitionSummary;
use clp_s_exec::ReadError;
use clp_s_exec::ReaderContext;
use clp_s_exec::RecordBatch;
use clp_s_exec::ResultOrder;
use clp_s_exec::ScanRequest;
use clp_s_exec::SearchSession;
use clp_s_exec::Sink;
use clp_s_exec::SinkControl;
use clp_s_exec::SinkError;
use clp_s_query::ColumnPattern;
use clp_s_query::Query;
use clp_s_query::QueryOptions;
use clp_s_query::TimeBounds;
use clp_s_resources::CancellationToken;
use clp_s_resources::ErrorKind;
use clp_s_search::RecordProjection;
use clp_s_search::TerminationStage;

/// The C++ outcome of one non-divergent case of `tests/queries/suite.toml`.
#[derive(Clone, Copy, Debug)]
struct SuiteCase {
    id: &'static str,
    archive: &'static str,
    kql: &'static str,
    is_case_insensitive: bool,
    time_begin_ms: Option<i64>,
    time_end_ms: Option<i64>,
    projection: &'static [&'static str],
}

/// The case fields every test overrides.
const BASE_CASE: SuiteCase = SuiteCase {
    id: "",
    archive: "search.ts",
    kql: "*",
    is_case_insensitive: false,
    time_begin_ms: None,
    time_end_ms: None,
    projection: &[],
};

/// Non-divergent single-archive cases of the suite, covering every leaf kernel family.
const SUITE_CASES: &[SuiteCase] = &[
    SuiteCase {
        id: "eq-var",
        kql: "level: INFO",
        ..BASE_CASE
    },
    SuiteCase {
        id: "range-int-gt",
        kql: "n > 10",
        ..BASE_CASE
    },
    SuiteCase {
        id: "clp-inner-wildcards",
        kql: r#"msg: "request * failed with status 50?""#,
        ..BASE_CASE
    },
    SuiteCase {
        id: "bool-and-not-mixed",
        kql: "(level: INFO OR level: WARN) AND NOT b: true",
        ..BASE_CASE
    },
    SuiteCase {
        id: "array-object-braces-and",
        kql: "objs: {k: tier AND v: 33}",
        ..BASE_CASE
    },
    SuiteCase {
        id: "partial-wildcard-deep",
        kql: "deep.*.d: 31",
        ..BASE_CASE
    },
    SuiteCase {
        id: "ci-clp-substr",
        kql: r#"msg: "*mixed case*""#,
        is_case_insensitive: true,
        ..BASE_CASE
    },
    SuiteCase {
        id: "time-window",
        time_begin_ms: Some(1_700_000_010_000),
        time_end_ms: Some(1_700_000_012_000),
        ..BASE_CASE
    },
    SuiteCase {
        id: "time-lower-and-filter",
        kql: "level: INFO",
        time_begin_ms: Some(1_700_000_030_000),
        ..BASE_CASE
    },
    SuiteCase {
        id: "proj-missing-columns",
        kql: "rid: r24 OR rid: r33 OR rid: r16",
        projection: &["p", "objs.k", "req", "obj", "nosuch", "rid"],
        ..BASE_CASE
    },
    SuiteCase {
        id: "logs-var",
        archive: "logs.ts",
        kql: "level: ERROR",
        ..BASE_CASE
    },
    SuiteCase {
        id: "logs-clp",
        archive: "logs.ts",
        kql: r#"msg: "*failed with status 50?*""#,
        ..BASE_CASE
    },
    SuiteCase {
        id: "logs-range",
        archive: "logs.ts-tables4k",
        kql: "latency_ms > 9900",
        ..BASE_CASE
    },
    SuiteCase {
        id: "many-schemas-nested-range",
        archive: "many-schemas.tables256",
        kql: "nested.lvl.k0 > 500",
        ..BASE_CASE
    },
];

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

/// # Returns
///
/// The sorted lines of the C++ stdout oracle of suite case `id`.
fn query_oracle_lines(id: &str) -> Vec<Vec<u8>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/queries/oracles")
        .join(format!("{id}.jsonl"));
    sorted_lines(&std::fs::read(path).expect("the query oracle is readable"))
}

/// # Returns
///
/// The context with `batch_num_rows` rows per window and `max_rendered_batch_size` bytes per
/// batch.
fn context(batch_num_rows: u32, max_rendered_batch_size: u64) -> Arc<ReaderContext> {
    let mut config = ExecutionConfig::default();
    config.batch_num_rows = NonZeroU32::new(batch_num_rows).expect("non-zero");
    config.max_rendered_batch_size = max_rendered_batch_size;
    ReaderContext::builder()
        .config(config)
        .build()
        .expect("the configuration is valid")
}

/// # Returns
///
/// The JSON request of `case`, on success.
///
/// # Errors
///
/// Returns an error if the query, a projected column or the request is invalid.
fn request(case: &SuiteCase) -> anyhow::Result<Arc<ScanRequest>> {
    let options = QueryOptions::default().with_case_sensitivity(!case.is_case_insensitive);
    let projection = if case.projection.is_empty() {
        RecordProjection::Full
    } else {
        RecordProjection::Columns(
            case.projection
                .iter()
                .map(|column| ColumnPattern::parse(column))
                .collect::<Result<_, _>>()?,
        )
    };
    Ok(Arc::new(
        ScanRequest::builder()
            .query(Query::from_kql(case.kql, &options)?)
            .time_bounds(TimeBounds::from_epoch_millis(
                case.time_begin_ms,
                case.time_end_ms,
            ))
            .projection(projection)
            .outputs(OutputRequirements::json().with_timestamps())
            .order(ResultOrder::Scan)
            .build()?,
    ))
}

/// # Returns
///
/// The archive of fixture `name`, opened with `context`, on success.
///
/// # Errors
///
/// Returns an error if the archive does not open.
fn open(context: &Arc<ReaderContext>, name: &str) -> Result<Archive, ReadError> {
    Archive::open(
        Arc::clone(context),
        ArchiveId::from(format!("{name}.clps")),
        ArchiveLocation::File(fixture(name)),
    )
}

/// # Returns
///
/// The concatenated JSON lines of every batch of a scan of `archive` with `request`, checking
/// that every batch is non-empty and fits the configured size (one record may exceed it), on
/// success.
///
/// # Errors
///
/// Returns an error if the scan fails.
fn scan_jsonl(
    archive: &Archive,
    request: Arc<ScanRequest>,
    max_rendered_batch_size: u64,
) -> Result<Vec<u8>, ReadError> {
    let mut scan = archive.scan(request, CancellationToken::new())?;
    let mut jsonl = Vec::new();
    while let Some(batch) = scan.next_batch()? {
        let json = batch.json().expect("JSON was requested");
        assert!(0 < batch.num_records());
        assert_eq!(json.len(), batch.num_records());
        let last_record_size = json
            .record(json.len() - 1)
            .map_or(0, |record| record.len() as u64 + 1);
        assert!(
            json.byte_size() - last_record_size < max_rendered_batch_size,
            "a batch of {} bytes exceeds {max_rendered_batch_size} bytes before its last record",
            json.byte_size()
        );
        assert_eq!(
            batch.timestamps_ns().map(<[Option<i64>]>::len),
            Some(batch.num_records())
        );
        jsonl.extend_from_slice(json.as_jsonl());
    }
    Ok(jsonl)
}

/// Collects the JSON lines of a session, partition by partition.
#[derive(Debug)]
struct JsonlSink;

impl Sink for JsonlSink {
    type Output = Vec<u8>;
    type Partial = JsonlPartial;

    fn requirements(&self) -> OutputRequirements {
        OutputRequirements::json()
    }

    fn merge_order(&self) -> MergeOrder {
        MergeOrder::Scan
    }

    fn open_partial(&self, _partition: &PartitionInfo) -> Result<Self::Partial, SinkError> {
        Ok(JsonlPartial { jsonl: Vec::new() })
    }

    fn merge(&self, output: &mut Vec<u8>, partial: Vec<u8>) -> Result<SinkControl, SinkError> {
        output.extend(partial);
        Ok(SinkControl::Continue)
    }
}

/// The partial sink of [`JsonlSink`].
#[derive(Debug)]
struct JsonlPartial {
    jsonl: Vec<u8>,
}

impl PartialSink for JsonlPartial {
    type Output = Vec<u8>;

    fn consume(&mut self, batch: &RecordBatch<'_>) -> Result<SinkControl, SinkError> {
        let json = batch
            .json()
            .ok_or_else(|| SinkError::Encode("the batch has no JSON".into()))?;
        self.jsonl.extend_from_slice(json.as_jsonl());
        Ok(SinkControl::Continue)
    }

    fn finish(self, _summary: &PartitionSummary) -> Result<Self::Output, SinkError> {
        Ok(self.jsonl)
    }
}

#[test]
fn filtered_scans_reproduce_the_cpp_query_oracles() -> anyhow::Result<()> {
    for (batch_num_rows, max_rendered_batch_size) in [(4096, 256 * 1024), (7, 1), (64, 300)] {
        let context = context(batch_num_rows, max_rendered_batch_size);
        for case in SUITE_CASES {
            let archive = open(&context, case.archive)?;
            let jsonl = scan_jsonl(&archive, request(case)?, max_rendered_batch_size)?;
            assert_eq!(
                sorted_lines(&jsonl),
                query_oracle_lines(case.id),
                "{} with windows of {batch_num_rows} rows and batches of \
                 {max_rendered_batch_size} bytes",
                case.id
            );
        }
    }
    Ok(())
}

#[test]
fn tables_kept_past_their_batches_leave_the_results_unchanged() -> anyhow::Result<()> {
    let context = context(7, 256 * 1024);
    for name in [
        "timestamps-int.default",
        "timestamps-int.ts",
        "timestamps-int.ts-nolog",
    ] {
        let archive = open(&context, name)?;
        let scan_records = |is_keeping_tables: bool| -> anyhow::Result<Vec<String>> {
            let request = ScanRequest::builder()
                .query(Query::from_kql("ts > 0", &QueryOptions::default())?)
                .outputs(OutputRequirements::json().with_timestamps())
                .build()?;
            let mut scan = archive.scan(Arc::new(request), CancellationToken::new())?;
            let mut kept_tables = Vec::new();
            let mut records = Vec::new();
            while let Some(batch) = scan.next_batch()? {
                let json = batch.json().expect("JSON was requested");
                let timestamps = batch.timestamps_ns().expect("timestamps were requested");
                for (index, timestamp) in timestamps.iter().enumerate() {
                    let record = json.record(index).expect("every record is rendered");
                    records.push(format!("{} {timestamp:?}", String::from_utf8_lossy(record)));
                }
                if let BatchRows::Table {
                    batch: table_batch, ..
                } = batch.rows()
                    && is_keeping_tables
                {
                    kept_tables.push(Arc::clone(table_batch.table()));
                }
            }
            assert_eq!(kept_tables.is_empty(), !is_keeping_tables);
            Ok(records)
        };
        let expected = scan_records(false)?;
        assert!(!expected.is_empty(), "{name}");
        assert_eq!(scan_records(true)?, expected, "{name}");
    }
    Ok(())
}

#[test]
fn sessions_over_split_archives_reproduce_the_cpp_query_oracles() -> anyhow::Result<()> {
    let context = context(4096, 256 * 1024);
    for case in [
        SuiteCase {
            id: "logs-split-var",
            kql: "level: ERROR AND service: auth",
            ..BASE_CASE
        },
        SuiteCase {
            id: "logs-time",
            time_begin_ms: Some(1_735_690_000_000),
            time_end_ms: Some(1_735_690_100_000),
            ..BASE_CASE
        },
        SuiteCase {
            id: "logs-split-array",
            kql: "tags: a",
            ..BASE_CASE
        },
    ] {
        let report = (0..9)
            .fold(
                SearchSession::builder(Arc::clone(&context), request(&case)?),
                |builder, index| {
                    let name = format!("logs.ts-split.{index}");
                    builder.archive(
                        ArchiveId::from(format!("{name}.clps")),
                        ArchiveLocation::File(fixture(&name)),
                    )
                },
            )
            .build()?
            .run(Arc::new(JsonlSink))?;
        assert_eq!(report.archive_summaries.len(), 9);
        assert_eq!(
            sorted_lines(&report.output),
            query_oracle_lines(case.id),
            "{}",
            case.id
        );
    }
    Ok(())
}

#[test]
fn match_all_with_a_projection_renders_only_the_projected_leaves() -> anyhow::Result<()> {
    let context = context(4096, 256 * 1024);
    let archive = open(&context, "search.ts")?;
    let case = SuiteCase {
        projection: &["rid"],
        ..BASE_CASE
    };
    let jsonl = scan_jsonl(&archive, request(&case)?, 256 * 1024)?;
    let expected: Vec<Vec<u8>> = sorted_lines(
        &(0..40)
            .flat_map(|rid| format!("{{\"rid\":\"r{rid:02}\"}}\n").into_bytes())
            .collect::<Vec<u8>>(),
    );
    assert_eq!(sorted_lines(&jsonl), expected);

    let full = scan_jsonl(&archive, request(&BASE_CASE)?, 256 * 1024)?;
    assert_eq!(sorted_lines(&full), query_oracle_lines("match-all"));
    Ok(())
}

#[test]
fn match_all_tables_decode_no_evaluation_column() -> anyhow::Result<()> {
    let context = context(4096, 256 * 1024);
    let archive = open(&context, "search.ts")?;
    let num_timestamped_records = query_oracle_lines("exists-ts").len();
    for (kql, is_match_all) in [
        ("ts >= 1600000000000", true),
        ("ts >= 1700000020000", false),
    ] {
        let json_request = Arc::new(
            ScanRequest::builder()
                .query(Query::from_kql(kql, &QueryOptions::default())?)
                .projection(RecordProjection::Columns(vec![ColumnPattern::parse(
                    "rid",
                )?]))
                .outputs(OutputRequirements::json())
                .build()?,
        );
        let mut scan = archive.scan(json_request, CancellationToken::new())?;
        let mut num_records = 0;
        while let Some(batch) = scan.next_batch()? {
            num_records += batch.num_records();
            let BatchRows::Table {
                batch: table_batch, ..
            } = batch.rows()
            else {
                anyhow::bail!("a scan delivers table batches");
            };
            let timestamp_column = table_batch
                .table()
                .layout()
                .record_timestamp_column()
                .expect("every matching table has the timestamp column");
            assert_eq!(
                table_batch.is_decoded(timestamp_column),
                !is_match_all,
                "{kql}: the timestamp column is decoded only to evaluate a residual filter"
            );
        }
        if is_match_all {
            assert_eq!(num_records, num_timestamped_records, "{kql}");
        } else {
            assert!(
                0 < num_records && num_records < num_timestamped_records,
                "{kql}"
            );
        }
    }
    Ok(())
}

#[test]
fn metadata_and_dictionary_pruning_read_no_stream() -> anyhow::Result<()> {
    let context = context(4096, 256 * 1024);
    let archive = open(&context, "logs.ts")?;
    for (kql, time_begin_ms, expected_stage) in [
        (
            "*",
            Some(1_800_000_000_000),
            TerminationStage::TimeRangeMatching,
        ),
        (
            "level: NO_SUCH_LEVEL",
            None,
            TerminationStage::DictionarySearch,
        ),
        ("nosuchkey: 1", None, TerminationStage::SchemaMatching),
    ] {
        let case = SuiteCase {
            archive: "logs.ts",
            kql,
            time_begin_ms,
            ..BASE_CASE
        };
        let report = SearchSession::builder(Arc::clone(&context), request(&case)?)
            .archive(
                ArchiveId::from("logs.ts.clps"),
                ArchiveLocation::File(fixture("logs.ts")),
            )
            .build()?
            .run(Arc::new(JsonlSink))?;
        assert_eq!(report.output, Vec::<u8>::new(), "{kql}");
        let summary = &report.archive_summaries[0];
        assert_eq!(summary.termination_stage, Some(expected_stage), "{kql}");
        assert_eq!(summary.stats.num_streams_decoded, 0, "{kql}");
        assert_eq!(summary.stats.num_tables_planned, 0, "{kql}");
        let mut scan = archive.scan(request(&case)?, CancellationToken::new())?;
        assert_eq!(scan.next_batch()?.map(|batch| batch.num_records()), None);
    }
    Ok(())
}

#[test]
fn wildcard_projections_fail_with_invalid_argument() -> anyhow::Result<()> {
    let context = context(4096, 256 * 1024);
    for projection in [&["*"][..], &["a.*.b"]] {
        let case = SuiteCase {
            archive: "logs.ts",
            projection,
            ..BASE_CASE
        };
        let error = SearchSession::builder(Arc::clone(&context), request(&case)?)
            .archive(
                ArchiveId::from("logs.ts.clps"),
                ArchiveLocation::File(fixture("logs.ts")),
            )
            .build()?
            .run(Arc::new(JsonlSink))
            .expect_err("the projection is invalid");
        assert_eq!(error.kind(), ErrorKind::InvalidArgument, "{error}");
    }

    let case = SuiteCase {
        archive: "logs.ts",
        kql: "level: ERROR",
        projection: &["lev*"],
        ..BASE_CASE
    };
    let archive = open(&context, "logs.ts")?;
    let jsonl = scan_jsonl(&archive, request(&case)?, 256 * 1024)?;
    assert_eq!(
        sorted_lines(&jsonl),
        vec![b"{}".to_vec(); query_oracle_lines("logs-var").len()],
        "a star inside a key is literal, as in C++"
    );
    Ok(())
}

#[test]
fn repeated_projection_columns_print_once() -> anyhow::Result<()> {
    let context = context(4096, 256 * 1024);
    let archive = open(&context, "logs.ts")?;
    let jsonl = |projection| -> anyhow::Result<Vec<u8>> {
        let case = SuiteCase {
            archive: "logs.ts",
            kql: "level: ERROR",
            projection,
            ..BASE_CASE
        };
        Ok(scan_jsonl(&archive, request(&case)?, 256 * 1024)?)
    };
    let repeated = jsonl(&["level", "msg", "level"])?;
    assert_eq!(
        sorted_lines(&repeated),
        sorted_lines(&jsonl(&["level", "msg"])?)
    );
    assert_eq!(
        sorted_lines(&repeated).len(),
        query_oracle_lines("logs-var").len()
    );
    Ok(())
}
