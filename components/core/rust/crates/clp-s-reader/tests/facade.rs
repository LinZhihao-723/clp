//! The facade end to end: open a C++ fixture archive from memory and from a host-provided
//! source, decompress it through the pull cursor, and take its latest records.

use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Arc;

use clp_s_reader::Archive;
use clp_s_reader::ArchiveId;
use clp_s_reader::ArchiveLocation;
use clp_s_reader::ByteSource;
use clp_s_reader::CancellationToken;
use clp_s_reader::ErrorKind;
use clp_s_reader::LatestFirstKey;
use clp_s_reader::OutputRequirements;
use clp_s_reader::ReadContext;
use clp_s_reader::ReaderContext;
use clp_s_reader::ResultOrder;
use clp_s_reader::ScanRequest;
use clp_s_reader::SharedBytes;
use clp_s_reader::SourceCapabilities;

/// A host-provided source over bytes held in memory, reporting itself as remote.
#[derive(Debug)]
struct HostSource {
    bytes: Vec<u8>,
}

impl ByteSource for HostSource {
    fn capabilities(&self) -> SourceCapabilities {
        SourceCapabilities {
            is_resident: false,
            is_remote: true,
        }
    }

    fn size(&self) -> Result<u64, std::io::Error> {
        Ok(self.bytes.len() as u64)
    }

    fn read_exact_at(
        &self,
        offset: u64,
        buffer: &mut [u8],
        context: &ReadContext<'_>,
    ) -> Result<(), std::io::Error> {
        context.check()?;
        let begin = usize::try_from(offset).map_err(std::io::Error::other)?;
        let source = begin
            .checked_add(buffer.len())
            .and_then(|end| self.bytes.get(begin..end))
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::UnexpectedEof))?;
        buffer.copy_from_slice(source);
        Ok(())
    }
}

/// # Returns
///
/// The records of `archive`, sorted, on success.
///
/// # Errors
///
/// Returns an error if the scan fails.
fn sorted_records(archive: &Archive) -> anyhow::Result<Vec<Vec<u8>>> {
    let request = ScanRequest::builder()
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
fn memory_and_host_sources_decompress_to_the_cpp_oracle() -> anyhow::Result<()> {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
    let bytes = std::fs::read(fixtures.join("archives/arrays.default.clps"))?;
    let mut expected: Vec<Vec<u8>> = std::fs::read(fixtures.join("oracles/arrays.default.jsonl"))?
        .split(|byte| b'\n' == *byte)
        .filter(|line| !line.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    expected.sort();

    let context = ReaderContext::builder().build()?;
    let memory = Archive::open(
        Arc::clone(&context),
        ArchiveId::from("arrays.default.clps"),
        ArchiveLocation::Memory(SharedBytes::from(bytes.clone())),
    )?;
    assert_eq!(sorted_records(&memory)?, expected);
    let host = Archive::open(
        Arc::clone(&context),
        ArchiveId::from("arrays.default.clps"),
        ArchiveLocation::Source(Arc::new(HostSource {
            bytes: bytes.clone(),
        })),
    )?;
    assert_eq!(sorted_records(&host)?, expected);

    let truncated = Archive::open(
        context,
        ArchiveId::from("truncated.clps"),
        ArchiveLocation::Source(Arc::new(HostSource {
            bytes: bytes[..64].to_vec(),
        })),
    )
    .expect_err("a truncated archive does not open");
    assert_eq!(truncated.kind(), ErrorKind::CorruptArchive, "{truncated}");
    Ok(())
}

#[test]
fn latest_first_scans_deliver_the_latest_records_once() -> anyhow::Result<()> {
    let bytes = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/archives/logs.ts.clps"),
    )?;
    let archive = Archive::open(
        ReaderContext::builder().build()?,
        ArchiveId::from("logs.ts.clps"),
        ArchiveLocation::Memory(SharedBytes::from(bytes)),
    )?;
    let keys = |request: ScanRequest| -> anyhow::Result<Vec<LatestFirstKey>> {
        let mut scan = archive.scan(Arc::new(request), CancellationToken::new())?;
        let mut keys = Vec::new();
        while let Some(batch) = scan.next_batch()? {
            let timestamps_ns = batch.timestamps_ns().expect("timestamps were requested");
            keys.extend((0..batch.num_records()).map(|record| LatestFirstKey {
                timestamp_ms: timestamps_ns[record].map(LatestFirstKey::timestamp_ms_of),
                archive_id: batch.archive_id(record).clone(),
                record_id: batch.record_ids()[record],
            }));
        }
        Ok(keys)
    };
    let mut expected = keys(
        ScanRequest::builder()
            .outputs(OutputRequirements::none().with_timestamps())
            .build()?,
    )?;
    expected.sort();
    expected.truncate(3);
    let latest = keys(
        ScanRequest::builder()
            .outputs(OutputRequirements::none().with_timestamps())
            .order(ResultOrder::LatestFirst {
                limit: NonZeroU32::new(3).expect("non-zero"),
            })
            .build()?,
    )?;
    assert_eq!(latest, expected);
    Ok(())
}
