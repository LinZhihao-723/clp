//! The msgpack result sink driven by sessions over C++ fixture archives: the file and network
//! outputs reproduce the C++ `clp-s s ... file|network` tuples, one writer serves the whole
//! session (DV-21, DV-22), missing timestamps and log order are written as 0 like C++, closed
//! peers surface as `SinkError::DownstreamClosed`, and top-k winners arrive sorted.

use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::net::TcpStream;
use std::num::NonZeroU32;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use clp_s_reader::ArchiveId;
use clp_s_reader::ArchiveLocation;
use clp_s_reader::Query;
use clp_s_reader::QueryOptions;
use clp_s_reader::ReadError;
use clp_s_reader::ReaderContext;
use clp_s_reader::ResultOrder;
use clp_s_reader::ScanRequest;
use clp_s_reader::SearchSession;
use clp_s_reader::SessionReport;
use clp_s_reader::SinkError;
use clp_s_sinks::MsgpackResultSink;
use parking_lot::Mutex;

/// One decoded tuple: `(timestamp_ms, message, orig_file_path, archive_id, log_event_idx)`.
type Tuple = (i64, String, String, String, i64);

/// # Returns
///
/// The prototype root.
fn prototype_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// # Returns
///
/// The path of fixture archive `name`.
fn fixture(name: &str) -> PathBuf {
    prototype_root()
        .join("tests/fixtures/archives")
        .join(format!("{name}.clps"))
}

/// # Returns
///
/// The raw C++ msgpack oracle of suite case `id` on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`std::fs::read`]'s return values on failure.
fn msgpack_oracle(id: &str) -> anyhow::Result<Vec<u8>> {
    Ok(std::fs::read(
        prototype_root()
            .join("tests/queries/oracles/msgpack")
            .join(format!("{id}.msgpack")),
    )?)
}

/// # Returns
///
/// The tuples of the concatenated msgpack tuples `data` on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`serde::Deserialize::deserialize`]'s return values on failure (`data` is not a
///   sequence of tuples).
fn decode(data: &[u8]) -> anyhow::Result<Vec<Tuple>> {
    let mut tuples = Vec::new();
    let mut remaining = data;
    while !remaining.is_empty() {
        let mut deserializer = rmp_serde::Deserializer::new(&mut remaining);
        tuples.push(serde::Deserialize::deserialize(&mut deserializer)?);
    }
    Ok(tuples)
}

/// # Returns
///
/// `tuples`, sorted.
fn sorted(mut tuples: Vec<Tuple>) -> Vec<Tuple> {
    tuples.sort();
    tuples
}

/// A writer appending to a buffer the test keeps.
#[derive(Clone, Debug, Default)]
struct SharedBuffer {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl Write for SharedBuffer {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.bytes.lock().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A writer whose writes fail with an error of `kind`.
#[derive(Debug)]
struct FailingWriter {
    kind: std::io::ErrorKind,
}

impl Write for FailingWriter {
    fn write(&mut self, _data: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::from(self.kind))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// # Returns
///
/// The report of a session running `kql` in `order` over the fixtures `names` into a msgpack
/// sink on `writer`, on success.
///
/// # Errors
///
/// Returns an error if the session fails.
fn run_session<WriterType: Write + Send + 'static>(
    names: &[&str],
    kql: &str,
    order: ResultOrder,
    writer: WriterType,
) -> Result<SessionReport<u64>, ReadError> {
    let context = ReaderContext::builder().build()?;
    let request = ScanRequest::builder()
        .query(Query::from_kql(kql, &QueryOptions::default())?)
        .order(order)
        .build()?;
    names
        .iter()
        .fold(
            SearchSession::builder(context, Arc::new(request)),
            |builder, name| {
                builder.archive(
                    ArchiveId::from(format!("{name}.clps")),
                    ArchiveLocation::File(fixture(name)),
                )
            },
        )
        .build()?
        .run(Arc::new(MsgpackResultSink::new(writer)))
}

/// # Returns
///
/// Whether `error` is a sink's [`SinkError::DownstreamClosed`].
fn is_downstream_closed(error: &ReadError) -> bool {
    matches!(error, ReadError::Sink(sink) if matches!(**sink, SinkError::DownstreamClosed))
}

/// # Returns
///
/// The msgpack bytes a session running `kql` over the fixtures `names` writes into a buffer, on
/// success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`run_session`]'s return values on failure.
/// * Forwards [`decode`]'s return values on failure.
fn file_output(names: &[&str], kql: &str, order: ResultOrder) -> anyhow::Result<Vec<u8>> {
    let buffer = SharedBuffer::default();
    let report = run_session(names, kql, order, buffer.clone())?;
    let bytes = buffer.bytes.lock().clone();
    assert_eq!(report.output, decode(&bytes)?.len() as u64);
    Ok(bytes)
}

#[test]
fn single_archive_outputs_are_the_cpp_bytes() -> anyhow::Result<()> {
    for (id, name, kql) in [
        ("wire-msgpack-search", "search.ts", "level: WARN"),
        ("wire-msgpack-nolog", "search.ts-nolog", "b: true"),
        ("wire-msgpack-kvir", "kvir-text.ts", "*"),
    ] {
        let bytes = file_output(&[name], kql, ResultOrder::Scan)?;
        let oracle = msgpack_oracle(id)?;
        assert_eq!(sorted(decode(&bytes)?), sorted(decode(&oracle)?), "{id}");
        assert_eq!(bytes, oracle, "{id}: the tuples are written in C++ order");
    }
    Ok(())
}

#[test]
fn archives_without_log_order_or_timestamps_write_zero() -> anyhow::Result<()> {
    let nolog = decode(&file_output(
        &["search.ts-nolog"],
        "b: true",
        ResultOrder::Scan,
    )?)?;
    assert!(!nolog.is_empty());
    assert!(nolog.iter().all(|tuple| 0 == tuple.4), "{nolog:?}");

    let untimed = decode(&file_output(
        &["search.default"],
        "level: WARN",
        ResultOrder::Scan,
    )?)?;
    let timed = decode(&msgpack_oracle("wire-msgpack-search")?)?;
    assert_eq!(untimed.len(), timed.len());
    for (untimed, timed) in untimed.iter().zip(&timed) {
        assert_eq!(untimed.0, 0, "{untimed:?}");
        assert_eq!(untimed.1, timed.1);
        assert_eq!(untimed.3, "search.default.clps");
        assert_eq!(untimed.4, timed.4);
    }
    Ok(())
}

#[test]
fn one_writer_holds_every_archive_of_the_session() -> anyhow::Result<()> {
    let names: Vec<String> = (0..9)
        .map(|index| format!("logs.ts-split.{index}"))
        .collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let tuples = decode(&file_output(
        &names,
        "level: ERROR AND service: auth",
        ResultOrder::Scan,
    )?)?;
    let stdout =
        std::fs::read(prototype_root().join("tests/queries/oracles/logs-split-var.jsonl"))?;
    let mut expected: Vec<String> = String::from_utf8(stdout)?
        .lines()
        .map(|line| format!("{line}\n"))
        .collect();
    expected.sort();
    let mut messages: Vec<String> = tuples.iter().map(|tuple| tuple.1.clone()).collect();
    messages.sort();
    assert_eq!(messages, expected);

    let cpp = decode(&msgpack_oracle("wire-msgpack-split")?)?;
    assert!(
        cpp.len() < tuples.len(),
        "C++ truncates the file per archive (DV-21)"
    );
    for tuple in &cpp {
        assert!(tuples.contains(tuple), "{tuple:?}");
    }
    let archive_ids: Vec<&str> = tuples.iter().map(|tuple| tuple.3.as_str()).collect();
    let mut sorted_ids = archive_ids.clone();
    sorted_ids.sort_unstable();
    assert_eq!(
        archive_ids, sorted_ids,
        "tuples follow the session's archive order"
    );
    Ok(())
}

#[test]
fn network_peers_receive_the_cpp_tuples_over_one_connection() -> anyhow::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let peer = std::thread::spawn(move || -> std::io::Result<(Vec<u8>, usize)> {
        let (mut stream, _) = listener.accept()?;
        let mut received = Vec::new();
        stream.read_to_end(&mut received)?;
        listener.set_nonblocking(true)?;
        let num_extra_connections = usize::from(listener.accept().is_ok());
        Ok((received, num_extra_connections))
    });
    let stream = TcpStream::connect(address)?;
    let report = run_session(
        &["search.ts", "search.ts-tables0"],
        "level: WARN",
        ResultOrder::Scan,
        stream,
    )?;
    let (received, num_extra_connections) = peer
        .join()
        .map_err(|_| anyhow::anyhow!("the peer thread panicked"))??;
    assert_eq!(num_extra_connections, 0);
    let tuples = decode(&received)?;
    assert_eq!(report.output, tuples.len() as u64);
    let oracle = msgpack_oracle("wire-msgpack-search")?;
    let (first, second) = tuples.split_at(decode(&oracle)?.len());
    assert_eq!(first, decode(&oracle)?);
    assert_eq!(received[..oracle.len()], oracle[..]);
    assert!(
        second
            .iter()
            .all(|tuple| tuple.3 == "search.ts-tables0.clps"),
        "{second:?}"
    );
    let renamed: Vec<Tuple> = second
        .iter()
        .map(|tuple| {
            (
                tuple.0,
                tuple.1.clone(),
                tuple.2.clone(),
                "search.ts.clps".to_owned(),
                tuple.4,
            )
        })
        .collect();
    assert_eq!(sorted(renamed), sorted(decode(&oracle)?));
    Ok(())
}

#[test]
fn closed_downstreams_fail_the_session() -> anyhow::Result<()> {
    for kind in [
        std::io::ErrorKind::BrokenPipe,
        std::io::ErrorKind::ConnectionReset,
        std::io::ErrorKind::ConnectionAborted,
    ] {
        let error = run_session(&["logs.ts"], "*", ResultOrder::Scan, FailingWriter { kind })
            .expect_err("the write fails");
        assert!(is_downstream_closed(&error), "{kind:?}: {error:?}");
    }
    let error = run_session(
        &["logs.ts"],
        "*",
        ResultOrder::Scan,
        FailingWriter {
            kind: std::io::ErrorKind::PermissionDenied,
        },
    )
    .expect_err("the write fails");
    assert!(
        matches!(&error, ReadError::Sink(sink) if matches!(**sink, SinkError::Io(_))),
        "{error:?}"
    );

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let peer = std::thread::spawn(move || listener.accept().map(drop));
    let stream = TcpStream::connect(address)?;
    peer.join()
        .map_err(|_| anyhow::anyhow!("the peer thread panicked"))??;
    let error = run_session(
        &["logs.ts", "logs.ts-tables4k", "logs.level1"],
        "*",
        ResultOrder::Scan,
        stream,
    )
    .expect_err("the peer closed the connection");
    assert!(is_downstream_closed(&error), "{error:?}");
    Ok(())
}

#[test]
fn top_k_winners_are_written_latest_first() -> anyhow::Result<()> {
    let limit = NonZeroU32::new(10).expect("non-zero");
    let tuples = decode(&file_output(
        &["logs.ts"],
        "*",
        ResultOrder::LatestFirst { limit },
    )?)?;
    let documents: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(
        prototype_root().join("tests/queries/oracles/results-cache/wire-cache-topk.json"),
    )?)?;
    let mut expected: Vec<Tuple> = documents
        .iter()
        .map(|document| {
            let integer = |value: &serde_json::Value| {
                value["$numberLong"]
                    .as_str()
                    .and_then(|text| text.parse::<i64>().ok())
                    .expect("an extended-JSON integer")
            };
            (
                integer(&document["timestamp"]),
                document["message"].as_str().expect("a message").to_owned(),
                String::new(),
                document["_id"]["archive_id"]
                    .as_str()
                    .expect("an archive id")
                    .to_owned(),
                integer(&document["_id"]["log_event_idx"]),
            )
        })
        .collect();
    expected.sort_by(|left, right| right.0.cmp(&left.0).then(left.4.cmp(&right.4)));
    assert_eq!(tuples, expected);
    Ok(())
}
