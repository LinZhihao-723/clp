//! Archives on S3 (feature `s3`), served by the in-process mock S3 server: every C++ fixture
//! archive decompresses and searches exactly like its local file, unsupported archives are still
//! rejected at open, and a scan blocked on a slow GET is cancelled in flight.

#![cfg(feature = "s3")]

use std::num::NonZeroU64;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use clp_s_object_store::test_util::MockBehavior;
use clp_s_object_store::test_util::MockS3Server;
use clp_s_reader::Archive;
use clp_s_reader::ArchiveId;
use clp_s_reader::ArchiveLocation;
use clp_s_reader::CancellationToken;
use clp_s_reader::ErrorKind;
use clp_s_reader::OutputRequirements;
use clp_s_reader::Query;
use clp_s_reader::QueryOptions;
use clp_s_reader::ReaderContext;
use clp_s_reader::ScanRequest;
use clp_s_reader::s3::S3Location;
use clp_s_reader::s3::S3Source;
use clp_s_reader::s3::S3SourceConfig;

const BUCKET: &str = "archives";

/// A multi-thread runtime and a mock server holding every fixture archive under its file name.
struct Fixture {
    server: MockS3Server,
    runtime: tokio::runtime::Runtime,
}

impl Fixture {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The fixture.
    ///
    /// # Panics
    ///
    /// Panics if the runtime or the server cannot be started, or a fixture cannot be read.
    fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("the runtime starts");
        let server = MockS3Server::start(runtime.handle()).expect("the mock server starts");
        for path in fixture_archives()
            .into_iter()
            .chain([fixtures_dir().join("reject/arrays.structurize-arrays.clps")])
        {
            server.put_object(
                BUCKET,
                &file_name(&path),
                std::fs::read(&path).expect("the fixture archive is readable"),
            );
        }
        Self { server, runtime }
    }

    /// # Returns
    ///
    /// The location of the S3 object `key` read with requests of at most `max_request_size`
    /// bytes, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`S3Source::new`]'s return values on failure.
    fn location(&self, key: &str, max_request_size: u64) -> anyhow::Result<ArchiveLocation> {
        let source = S3Source::new(
            self.server.client(),
            self.runtime.handle().clone(),
            S3Location {
                bucket: BUCKET.to_owned(),
                key: key.to_owned(),
            },
            S3SourceConfig {
                max_request_size: NonZeroU64::new(max_request_size)
                    .expect("tests use a non-zero request size"),
            },
        )?;
        Ok(ArchiveLocation::Source(Arc::new(source)))
    }
}

/// # Returns
///
/// The directory of the fixture archives.
fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/archives")
}

/// # Returns
///
/// The supported fixture archives, sorted.
///
/// # Panics
///
/// Panics if the fixture directory cannot be listed.
fn fixture_archives() -> Vec<PathBuf> {
    let mut archives: Vec<PathBuf> = std::fs::read_dir(fixtures_dir())
        .expect("the fixture directory is listable")
        .map(|entry| entry.expect("the fixture entry is readable").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| "clps" == extension)
        })
        .collect();
    archives.sort();
    archives
}

/// # Returns
///
/// The file name of `path`.
///
/// # Panics
///
/// Panics if `path` doesn't name a file.
fn file_name(path: &Path) -> String {
    path.file_name()
        .expect("fixture paths name a file")
        .to_string_lossy()
        .into_owned()
}

/// # Returns
///
/// The JSON records `archive` selects with `kql`, sorted, on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`Query::from_kql`]'s return values on failure.
/// * Forwards [`clp_s_reader::ScanRequestBuilder::build`]'s return values on failure.
/// * Forwards [`Archive::scan`]'s return values on failure.
/// * Forwards [`clp_s_reader::Scan::next_batch`]'s return values on failure.
fn sorted_records(archive: &Archive, kql: &str) -> anyhow::Result<Vec<Vec<u8>>> {
    let request = ScanRequest::builder()
        .query(Query::from_kql(kql, &QueryOptions::default())?)
        .outputs(OutputRequirements::json())
        .build()?;
    let mut scan = archive.scan(Arc::new(request), CancellationToken::new())?;
    let mut records = Vec::new();
    while let Some(batch) = scan.next_batch()? {
        let json = batch.json().expect("JSON was requested");
        records.extend((0..json.len()).map(|index| {
            json.record(index)
                .expect("every record is buffered")
                .to_vec()
        }));
    }
    records.sort();
    Ok(records)
}

#[test]
fn fixture_archives_on_s3_read_like_local_files() -> anyhow::Result<()> {
    const QUERIES: [&str; 5] = [
        "*",
        "n: 5",
        "level: ERROR OR s: \"*a*\"",
        "NOT n > 3",
        "msg: *",
    ];
    let fixture = Fixture::new();
    let context = ReaderContext::builder().build()?;
    let archives = fixture_archives();
    assert!(archives.len() > 40, "{archives:?}");
    for path in archives {
        let name = file_name(&path);
        let local = Archive::open(
            Arc::clone(&context),
            ArchiveId::from(name.as_str()),
            ArchiveLocation::File(path),
        )?;
        // Small requests split most streams into several GETs.
        for max_request_size in [1024, 64 * 1024 * 1024] {
            let remote = Archive::open(
                Arc::clone(&context),
                ArchiveId::from(name.as_str()),
                fixture.location(&name, max_request_size)?,
            )?;
            assert_eq!(remote.info(), local.info(), "{name}");
            for kql in QUERIES {
                assert_eq!(
                    sorted_records(&remote, kql)?,
                    sorted_records(&local, kql)?,
                    "{name}: {kql}"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn unsupported_archives_on_s3_are_rejected_at_open() -> anyhow::Result<()> {
    let fixture = Fixture::new();
    let error = Archive::open(
        ReaderContext::builder().build()?,
        ArchiveId::from("arrays.structurize-arrays.clps"),
        fixture.location("arrays.structurize-arrays.clps", 1024)?,
    )
    .expect_err("structured arrays are not supported");
    assert_eq!(error.kind(), ErrorKind::UnsupportedArchive, "{error}");
    Ok(())
}

#[test]
fn scans_waiting_on_s3_are_cancelled_in_flight() -> anyhow::Result<()> {
    let fixture = Fixture::new();
    let archive = Archive::open(
        ReaderContext::builder().build()?,
        ArchiveId::from("logs.ts.clps"),
        fixture.location("logs.ts.clps", 1024)?,
    )?;
    fixture.server.set_behavior(MockBehavior {
        get_delay_ms: 10_000,
        ..MockBehavior::default()
    });
    let token = CancellationToken::new();
    let canceller = {
        let token = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            token.cancel();
        })
    };
    let start = Instant::now();
    let request = ScanRequest::builder()
        .outputs(OutputRequirements::json())
        .build()?;
    let error = match archive.scan(Arc::new(request), token) {
        Ok(mut scan) => loop {
            match scan.next_batch() {
                Ok(Some(_)) => {}
                Ok(None) => anyhow::bail!("the scan finished although every GET is delayed"),
                Err(error) => break error,
            }
        },
        Err(error) => error,
    };
    let elapsed = start.elapsed();
    canceller
        .join()
        .map_err(|_| anyhow::anyhow!("the canceller panicked"))?;
    assert_eq!(error.kind(), ErrorKind::Cancelled, "{error}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    Ok(())
}
