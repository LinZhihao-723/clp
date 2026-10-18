//! The JSON-lines and collecting sinks driven by sessions (full scans and searches) over C++
//! fixture archives.

use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use clp_s_reader::ArchiveId;
use clp_s_reader::ArchiveLocation;
use clp_s_reader::ErrorKind;
use clp_s_reader::PartitionInfo;
use clp_s_reader::PartitionKind;
use clp_s_reader::Query;
use clp_s_reader::QueryOptions;
use clp_s_reader::ReadError;
use clp_s_reader::ReaderContext;
use clp_s_reader::ResultOrder;
use clp_s_reader::ScanRequest;
use clp_s_reader::SearchSession;
use clp_s_reader::SessionReport;
use clp_s_reader::Sink;
use clp_s_reader::SinkError;
use clp_s_sinks::CollectSink;
use clp_s_sinks::JsonLinesSink;
use parking_lot::Mutex;

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
/// The sorted C++ `x` oracle lines of fixture `name`.
fn oracle_lines(name: &str) -> Vec<Vec<u8>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/oracles")
        .join(format!("{name}.jsonl"));
    sorted_lines(&std::fs::read(path).expect("the oracle is readable"))
}

/// # Type Parameters
///
/// * `SinkType` - The sink the session pushes its records into.
///
/// # Returns
///
/// The report of a full-scan session over the fixtures `names` into `sink`, on success.
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
        .build()?
        .run(Arc::new(sink))
}

/// A writer appending to a buffer the test keeps.
#[derive(Clone, Debug, Default)]
struct SharedBuffer {
    bytes: Arc<Mutex<Vec<u8>>>,
    num_flushes: Arc<Mutex<usize>>,
}

impl Write for SharedBuffer {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.bytes.lock().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        *self.num_flushes.lock() += 1;
        Ok(())
    }
}

/// A writer whose writes or flushes fail with an error of `kind`.
#[derive(Debug)]
struct FailingWriter {
    kind: std::io::ErrorKind,
    is_write_failing: bool,
}

impl Write for FailingWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.is_write_failing {
            return Err(std::io::Error::from(self.kind));
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::from(self.kind))
    }
}

#[test]
fn json_lines_reproduce_the_cpp_oracles_in_session_order() -> anyhow::Result<()> {
    let buffer = SharedBuffer::default();
    let names = ["logs.ts-split.1", "logs.ts-split.0", "strings.default"];
    let report = run_session(&names, JsonLinesSink::new(buffer.clone()))?;
    let bytes = buffer.bytes.lock().clone();
    let num_records = report
        .archive_summaries
        .iter()
        .map(|summary| summary.stats.num_records_selected)
        .sum::<u64>();
    assert_eq!(report.output, num_records);
    assert_eq!(report.output, 336 + 336 + 30);
    assert!(bytes.ends_with(b"\n"));
    assert_eq!(*buffer.num_flushes.lock(), names.len());

    let mut expected: Vec<Vec<u8>> = names.iter().flat_map(|name| oracle_lines(name)).collect();
    expected.sort();
    assert_eq!(sorted_lines(&bytes), expected);

    let lines: Vec<&[u8]> = bytes.split(|byte| b'\n' == *byte).collect();
    let first_archive_lines = sorted_lines(&std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/oracles/logs.ts-split.1.jsonl"),
    )?);
    assert!(
        lines[..336]
            .iter()
            .all(|line| first_archive_lines.binary_search(&line.to_vec()).is_ok()),
        "the records of the first archive come first"
    );
    Ok(())
}

#[test]
fn broken_pipes_close_the_downstream() {
    let error = run_session(
        &["logs.ts"],
        JsonLinesSink::new(FailingWriter {
            kind: std::io::ErrorKind::BrokenPipe,
            is_write_failing: true,
        }),
    )
    .expect_err("the write fails");
    assert_eq!(error.kind(), ErrorKind::Sink, "{error}");
    assert!(
        matches!(&error, ReadError::Sink(sink) if matches!(**sink, SinkError::DownstreamClosed)),
        "{error:?}"
    );

    let error = run_session(
        &["logs.ts"],
        JsonLinesSink::new(FailingWriter {
            kind: std::io::ErrorKind::PermissionDenied,
            is_write_failing: true,
        }),
    )
    .expect_err("the write fails");
    assert!(
        matches!(&error, ReadError::Sink(sink) if matches!(**sink, SinkError::Io(_))),
        "{error:?}"
    );

    let error = run_session(
        &["logs.ts"],
        JsonLinesSink::new(FailingWriter {
            kind: std::io::ErrorKind::BrokenPipe,
            is_write_failing: false,
        }),
    )
    .expect_err("the flush fails");
    assert!(
        matches!(&error, ReadError::Sink(sink) if matches!(**sink, SinkError::DownstreamClosed)),
        "{error:?}"
    );
}

#[test]
fn collected_records_carry_ids_timestamps_and_indices() -> anyhow::Result<()> {
    let names = ["logs.ts-split.2", "scalars.nolog"];
    let report = run_session(&names, CollectSink::new())?;
    let records = report.output;
    assert_eq!(records.len(), 335 + 11);
    let (logs, scalars) = records.split_at(335);
    assert!(
        logs.iter()
            .all(|record| "logs.ts-split.2.clps" == record.archive_id.as_str())
    );
    assert!(
        scalars
            .iter()
            .all(|record| "scalars.nolog.clps" == record.archive_id.as_str())
    );
    for record in logs {
        let text = String::from_utf8(record.json.clone())?;
        let ts = text
            .split("\"ts\":")
            .nth(1)
            .and_then(|rest| rest.split([',', '}']).next())
            .and_then(|value| value.parse::<i64>().ok())
            .expect("logs records have an integer ts");
        assert_eq!(record.timestamp_ns, Some(ts * 1_000_000), "{text}");
        assert_eq!(record.log_event_idx, Some(record.record_id.get()), "{text}");
        assert!(!record.json.ends_with(b"\n"));
    }
    for record in scalars {
        assert_eq!(record.timestamp_ns, None);
        assert_eq!(record.log_event_idx, None);
    }
    let mut collected: Vec<Vec<u8>> = records.iter().map(|record| record.json.clone()).collect();
    collected.sort();
    let mut expected: Vec<Vec<u8>> = names.iter().flat_map(|name| oracle_lines(name)).collect();
    expected.sort();
    assert_eq!(collected, expected);
    Ok(())
}

#[test]
fn json_lines_of_a_search_session_are_taken_back_from_the_sink() -> anyhow::Result<()> {
    let context = ReaderContext::builder().build()?;
    let request = ScanRequest::builder()
        .query(Query::from_kql(
            "level: ERROR AND service: auth",
            &QueryOptions::default(),
        )?)
        .build()?;
    let sink = Arc::new(JsonLinesSink::new(Vec::new()));
    let report = (0..9)
        .fold(
            SearchSession::builder(context, Arc::new(request)),
            |builder, index| {
                let name = format!("logs.ts-split.{index}");
                builder.archive(
                    ArchiveId::from(format!("{name}.clps")),
                    ArchiveLocation::File(fixture(&name)),
                )
            },
        )
        .build()?
        .run(Arc::clone(&sink))?;
    let sink = Arc::try_unwrap(sink).map_err(|_| anyhow::anyhow!("the session keeps the sink"))?;
    let jsonl = sink
        .into_writer()
        .map_err(|_| anyhow::anyhow!("no partial sink outlives the session"))?;
    let oracle = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/queries/oracles/logs-split-var.jsonl"),
    )?;
    assert_eq!(sorted_lines(&jsonl), sorted_lines(&oracle));
    assert_eq!(report.output, sorted_lines(&oracle).len() as u64);
    Ok(())
}

#[test]
fn json_lines_writers_are_kept_while_a_partial_shares_them() -> anyhow::Result<()> {
    let sink = JsonLinesSink::new(Vec::<u8>::new());
    let partial = sink.open_partial(&PartitionInfo {
        index: 0,
        kind: PartitionKind::TopK,
        order: ResultOrder::Scan,
    })?;
    let Err(sink) = sink.into_writer() else {
        anyhow::bail!("a partial sink still shares the writer");
    };
    drop(partial);
    assert_eq!(
        sink.into_writer()
            .map_err(|_| anyhow::anyhow!("the writer is no longer shared"))?,
        Vec::<u8>::new()
    );
    Ok(())
}
