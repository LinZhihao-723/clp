//! Storage tests over the C++-written fixture corpus: every archive opened through file, memory and
//! host sources, every dictionary loaded and every stream decoded under each read policy, plus the
//! rejection, cancellation, retry and limit paths.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use clp_s_format::ByteRange;
use clp_s_format::DictionaryNeeds;
use clp_s_format::FormatError;
use clp_s_format::OpenOptions;
use clp_s_format::ReaderLimits;
use clp_s_format::SectionId;
use clp_s_format::SharedBytes;
use clp_s_format::StreamDecoder;
use clp_s_format::StreamRead;
use clp_s_format::TableBytes;
use clp_s_format::TableSet;
use clp_s_format::UnsupportedFeature;
use clp_s_io::ArchiveStorage;
use clp_s_io::ByteSource;
use clp_s_io::MemorySource;
use clp_s_io::ReadContext;
use clp_s_io::ResolvedSource;
use clp_s_io::SourceCapabilities;
use clp_s_io::StorageError;
use clp_s_io::StreamReadOptions;
use clp_s_resources::BufferPool;
use clp_s_resources::BufferPoolConfig;
use clp_s_resources::CancellationToken;
use clp_s_resources::ErrorKind;
use clp_s_resources::MemoryBudget;

const ALL_DICTIONARIES: DictionaryNeeds = DictionaryNeeds {
    is_variable_needed: true,
    is_logtype_needed: true,
    is_array_needed: true,
};

/// A host source over memory that is neither resident nor sliceable, counts its reads, can fail
/// the reads that touch one range, and can cancel a token after a number of reads. It ignores
/// cancellation itself, so only storage's own checks stop a cancelled read.
struct HostSource {
    bytes: MemorySource,
    capabilities: SourceCapabilities,
    num_reads: AtomicUsize,
    failing_range: Mutex<Option<ByteRange>>,
    cancel_after_reads: Option<(usize, CancellationToken)>,
}

impl HostSource {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A host source over `bytes` that reports itself as remote if `is_remote`.
    const fn new(bytes: SharedBytes, is_remote: bool) -> Self {
        Self {
            bytes: MemorySource::new(bytes),
            capabilities: SourceCapabilities {
                is_resident: false,
                is_remote,
            },
            num_reads: AtomicUsize::new(0),
            failing_range: Mutex::new(None),
            cancel_after_reads: None,
        }
    }

    /// Makes the reads that overlap `range` fail, or none if `range` is `None`.
    fn fail_reads_in(&self, range: Option<ByteRange>) {
        *self
            .failing_range
            .lock()
            .expect("the lock is never poisoned") = range;
    }
}

impl ByteSource for HostSource {
    fn capabilities(&self) -> SourceCapabilities {
        self.capabilities
    }

    fn size(&self) -> Result<u64, std::io::Error> {
        self.bytes.size()
    }

    fn read_exact_at(
        &self,
        offset: u64,
        buffer: &mut [u8],
        _context: &ReadContext<'_>,
    ) -> Result<(), std::io::Error> {
        let num_reads = self.num_reads.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some((limit, token)) = &self.cancel_after_reads
            && num_reads >= *limit
        {
            token.cancel();
        }
        let failing_range = *self
            .failing_range
            .lock()
            .expect("the lock is never poisoned");
        if let Some(range) = failing_range
            && offset < range.end
            && range.begin < offset + buffer.len() as u64
        {
            return Err(std::io::Error::other("injected read failure"));
        }
        self.bytes.read_exact_at(
            offset,
            buffer,
            &ReadContext {
                cancellation_token: &CancellationToken::new(),
            },
        )
    }
}

/// # Returns
///
/// The fixture corpus directory.
fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

/// # Returns
///
/// Every readable fixture archive, sorted.
fn readable_archives() -> Vec<PathBuf> {
    let mut archives: Vec<PathBuf> = std::fs::read_dir(fixtures_dir().join("archives"))
        .expect("the fixture directory is readable")
        .map(|entry| entry.expect("the entry is readable").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "clps")
        })
        .collect();
    archives.sort();
    archives
}

/// # Returns
///
/// The bytes of the file at `path`.
fn read_bytes(path: &Path) -> SharedBytes {
    SharedBytes::from(std::fs::read(path).expect("the fixture is readable"))
}

/// # Returns
///
/// The expected counts of every fixture (from the format crate's independent reading), keyed by
/// file name and then by column.
fn expected_counts() -> HashMap<String, HashMap<String, String>> {
    let text = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../clp-s-format/tests/data/fixture_counts.tsv"),
    )
    .expect("the counts file is readable");
    let mut lines = text.lines().filter(|line| !line.starts_with('#'));
    let header: Vec<String> = lines
        .next()
        .expect("the counts file has a header")
        .split('\t')
        .map(str::to_owned)
        .collect();
    lines
        .map(|line| {
            let row: HashMap<String, String> = header
                .iter()
                .cloned()
                .zip(line.split('\t').map(str::to_owned))
                .collect();
            (row["file"].clone(), row)
        })
        .collect()
}

/// Decodes `read` into a fresh pooled buffer.
///
/// # Returns
///
/// The decoded prefix on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`ArchiveStorage::read_stream`]'s return values on failure.
fn read_stream(
    storage: &ArchiveStorage,
    read: &StreamRead,
    options: &StreamReadOptions,
) -> Result<SharedBytes, StorageError> {
    let pool = BufferPool::new(BufferPoolConfig::default());
    let reservation = MemoryBudget::new(None)
        .try_reserve(read.decoded_prefix_size)
        .expect("the budget is unlimited");
    let buffer = pool
        .acquire(read.decoded_prefix_size, reservation)
        .expect("the buffer can be allocated");
    let mut decoder = StreamDecoder::new()?;
    storage.read_stream(read, &mut decoder, buffer, options)
}

/// # Returns
///
/// The read plan for every table of `storage`.
fn full_plan(storage: &ArchiveStorage) -> Vec<StreamRead> {
    let num_tables = u32::try_from(storage.index().tables().tables().len()).expect("few tables");
    storage
        .index()
        .plan_reads(&TableSet::full(num_tables), DictionaryNeeds::default())
        .streams
}

/// # Returns
///
/// The storage of the fixture `file` over a memory source, opened with default options.
fn open_memory(file: &str) -> ArchiveStorage {
    let bytes = read_bytes(&fixtures_dir().join("archives").join(file));
    ArchiveStorage::open(
        ResolvedSource::Memory(MemorySource::new(bytes)),
        &OpenOptions::default(),
        &CancellationToken::new(),
    )
    .expect("the fixture opens")
}

#[test]
fn every_fixture_reads_identically_through_every_source() -> anyhow::Result<()> {
    let expected = expected_counts();
    let token = CancellationToken::new();
    for path in readable_archives() {
        let bytes = read_bytes(&path);
        let name = path
            .file_name()
            .expect("archives have names")
            .to_string_lossy()
            .into_owned();
        let counts = &expected[&name];
        let sources = [
            ("file", ResolvedSource::from_path(&path)?),
            (
                "memory",
                ResolvedSource::Memory(MemorySource::new(bytes.clone())),
            ),
            (
                "remote",
                ResolvedSource::Dyn(Arc::new(HostSource::new(bytes.clone(), true))),
            ),
        ];
        let mut reference_streams: Option<Vec<SharedBytes>> = None;
        for (source_name, source) in sources {
            let is_file = matches!(source, ResolvedSource::File(_));
            let storage = ArchiveStorage::open(source, &OpenOptions::default(), &token)?;
            storage.load_dictionaries(ALL_DICTIONARIES, &token)?;
            let dictionaries = storage.dictionaries();
            let dictionary_sizes = [
                dictionaries
                    .variable
                    .get()
                    .map(|dictionary| dictionary.len()),
                dictionaries
                    .logtype
                    .get()
                    .map(|dictionary| dictionary.len()),
                dictionaries.array.get().map(|dictionary| dictionary.len()),
            ];
            let expected_sizes = ["num_variables", "num_logtypes", "num_arrays"]
                .map(|column| Some(counts[column].parse::<u64>().expect("a number")));
            assert_eq!(dictionary_sizes, expected_sizes, "{name} via {source_name}");
            assert_eq!(
                storage.index().tables().num_log_events().to_string(),
                counts["num_log_events"],
                "{name} via {source_name}"
            );

            let chunk_sizes: &[u64] = if is_file {
                &[1, 7, 4096, 1 << 20]
            } else {
                &[1 << 20]
            };
            for chunk_size in chunk_sizes {
                let options = StreamReadOptions {
                    chunk_size: *chunk_size,
                    cancellation_token: token.clone(),
                };
                let mut streams = Vec::new();
                let num_bytes_read_before = storage.num_bytes_read();
                let mut compressed_size = 0;
                for read in full_plan(&storage) {
                    compressed_size += read.stream.compressed_range.len();
                    let decoded = read_stream(&storage, &read, &options)?;
                    for ordinal in &read.tables {
                        let info = *storage
                            .index()
                            .tables()
                            .table(*ordinal)
                            .expect("planned tables exist");
                        TableBytes::new(&decoded, info)?;
                    }
                    streams.push(decoded);
                }
                assert_eq!(
                    storage.num_bytes_read() - num_bytes_read_before,
                    compressed_size,
                    "{name} via {source_name} reads every compressed byte once"
                );
                let decoded_size: usize = streams.iter().map(SharedBytes::len).sum();
                assert_eq!(
                    decoded_size.to_string(),
                    counts["decoded_stream_size"],
                    "{name}"
                );
                match &reference_streams {
                    Some(reference) => assert!(
                        *reference == streams,
                        "{name} via {source_name} with {chunk_size}-byte chunks decodes \
                         differently"
                    ),
                    None => reference_streams = Some(streams),
                }
            }
        }
    }
    Ok(())
}

#[test]
fn stepwise_reads_decode_like_whole_reads_and_restart_while_shared() -> anyhow::Result<()> {
    let token = CancellationToken::new();
    let pool = BufferPool::new(BufferPoolConfig::default());
    let budget = MemoryBudget::new(None);
    let acquire = |size: u64| {
        pool.acquire(
            size,
            budget.try_reserve(size).expect("the budget is unlimited"),
        )
        .expect("the buffer can be allocated")
    };
    let mut num_restarts = 0;
    for path in readable_archives() {
        let bytes = read_bytes(&path);
        let sources = [
            (ResolvedSource::from_path(&path)?, 7),
            (ResolvedSource::from_path(&path)?, 1 << 20),
            (
                ResolvedSource::Memory(MemorySource::new(bytes.clone())),
                1 << 20,
            ),
            (
                ResolvedSource::Dyn(Arc::new(HostSource::new(bytes.clone(), true))),
                1 << 20,
            ),
        ];
        for (source, chunk_size) in sources {
            let storage = ArchiveStorage::open(source, &OpenOptions::default(), &token)?;
            let options = StreamReadOptions {
                chunk_size,
                cancellation_token: token.clone(),
            };
            let mut decoder = StreamDecoder::new()?;
            for read in full_plan(&storage) {
                let whole = read_stream(&storage, &read, &options)?;
                let prefix_size = read.decoded_prefix_size;
                let mut reader =
                    storage.begin_stream(&read, &mut decoder, acquire(prefix_size), &options)?;
                let mut kept = None;
                for target in [prefix_size / 3, prefix_size / 2, prefix_size] {
                    if !reader.try_reclaim() {
                        reader.restart(&mut decoder, acquire(prefix_size))?;
                        num_restarts += 1;
                    }
                    storage.decode_stream_to(&mut reader, &mut decoder, target, &options)?;
                    let shared = reader.share();
                    assert!(shared.len() as u64 >= target, "{}", path.display());
                    assert_eq!(shared, whole.slice(..shared.len()), "{}", path.display());
                    kept = (target == prefix_size / 3 && target < prefix_size).then_some(shared);
                }
                drop(kept);
                assert!(reader.num_decoded_bytes() >= prefix_size);
                assert_eq!(reader.finish()?, whole, "{}", path.display());
            }
        }
    }
    assert!(num_restarts > 0);
    Ok(())
}

#[test]
fn remote_sources_read_each_stream_once() -> anyhow::Result<()> {
    let bytes = read_bytes(&fixtures_dir().join("archives/many-schemas.tables256.clps"));
    let source = Arc::new(HostSource::new(bytes.clone(), true));
    let token = CancellationToken::new();
    let storage = ArchiveStorage::open(
        ResolvedSource::Dyn(source.clone()),
        &OpenOptions::default(),
        &token,
    )?;
    assert_eq!(
        source.num_reads.load(Ordering::Relaxed),
        1,
        "one prefix read opens it"
    );
    assert_eq!(storage.num_bytes_read(), bytes.len() as u64);

    let plan = full_plan(&storage);
    let options = StreamReadOptions {
        chunk_size: 16,
        cancellation_token: token,
    };
    let mut compressed_size = 0;
    for read in &plan {
        read_stream(&storage, read, &options)?;
        compressed_size += read.stream.compressed_range.len();
    }
    assert_eq!(source.num_reads.load(Ordering::Relaxed), 1 + plan.len());
    assert_eq!(
        storage.num_bytes_read(),
        bytes.len() as u64 + compressed_size
    );
    Ok(())
}

#[test]
fn small_prefixes_fetch_the_metadata_and_schema_layer_separately() -> anyhow::Result<()> {
    let bytes = read_bytes(&fixtures_dir().join("archives/search.ts.clps"));
    let source = Arc::new(HostSource::new(bytes, false));
    let options = OpenOptions {
        prefix_fetch_size: 64,
        ..OpenOptions::default()
    };
    let storage = ArchiveStorage::open(
        ResolvedSource::Dyn(source.clone()),
        &options,
        &CancellationToken::new(),
    )?;
    assert_eq!(source.num_reads.load(Ordering::Relaxed), 3);
    assert_eq!(storage.index().tables().tables().len(), 34);
    Ok(())
}

#[test]
fn unsupported_archives_fail_at_open() {
    let error = ResolvedSource::from_path(&fixtures_dir().join("archives/reject/scalars.mfa"))
        .expect_err("a multi-file archive is out of scope");
    assert!(
        matches!(error, StorageError::MultiFileArchive { .. }),
        "{error:?}"
    );
    assert_eq!(error.kind(), ErrorKind::UnsupportedArchive);

    let path = fixtures_dir().join("archives/reject/arrays.structurize-arrays.clps");
    let source = ResolvedSource::from_path(&path).expect("the file opens");
    let error = ArchiveStorage::open(source, &OpenOptions::default(), &CancellationToken::new())
        .expect_err("structured arrays are out of scope");
    assert!(
        matches!(
            error,
            StorageError::Format(FormatError::UnsupportedFeature {
                feature: UnsupportedFeature::StructuredArrays,
                ..
            })
        ),
        "{error:?}"
    );
    assert_eq!(error.kind(), ErrorKind::UnsupportedArchive);
}

#[test]
fn cancelled_tokens_stop_every_read() -> anyhow::Result<()> {
    let path = fixtures_dir().join("archives/logs.ts.clps");
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let error = ArchiveStorage::open(
        ResolvedSource::from_path(&path)?,
        &OpenOptions::default(),
        &cancelled,
    )
    .expect_err("the token is cancelled");
    assert_eq!(error.kind(), ErrorKind::Cancelled);

    let storage = open_memory("logs.ts.clps");
    let error = storage
        .load_dictionaries(ALL_DICTIONARIES, &cancelled)
        .expect_err("the token is cancelled");
    assert_eq!(error.kind(), ErrorKind::Cancelled);
    assert_eq!(
        storage
            .dictionaries()
            .variable
            .get()
            .map(|dictionary| dictionary.len()),
        None
    );

    let read = &full_plan(&storage)[0];
    let options = StreamReadOptions {
        chunk_size: 1 << 20,
        cancellation_token: cancelled,
    };
    let error = read_stream(&storage, read, &options).expect_err("the token is cancelled");
    assert!(matches!(error, StorageError::Cancelled), "{error:?}");
    Ok(())
}

#[test]
fn cancelling_during_a_chunked_read_stops_it() -> anyhow::Result<()> {
    let bytes = read_bytes(&fixtures_dir().join("archives/logs.ts.clps"));
    let token = CancellationToken::new();
    let mut source = HostSource::new(bytes, false);
    source.cancel_after_reads = Some((3, token.clone()));
    let storage = ArchiveStorage::open(
        ResolvedSource::Dyn(Arc::new(source)),
        &OpenOptions::default(),
        &token,
    )?;
    let read = &full_plan(&storage)[0];
    let options = StreamReadOptions {
        chunk_size: 1024,
        cancellation_token: token,
    };
    let error = read_stream(&storage, read, &options).expect_err("the token is cancelled mid-read");
    assert!(matches!(error, StorageError::Cancelled), "{error:?}");
    Ok(())
}

#[test]
fn a_failed_dictionary_load_leaves_the_cell_empty_and_retries() -> anyhow::Result<()> {
    let bytes = read_bytes(&fixtures_dir().join("archives/search.ts.clps"));
    let source = Arc::new(HostSource::new(bytes, false));
    let token = CancellationToken::new();
    let storage = ArchiveStorage::open(
        ResolvedSource::Dyn(source.clone()),
        &OpenOptions::default(),
        &token,
    )?;
    let variable_range = storage
        .index()
        .metadata()
        .sections()
        .range(SectionId::VariableDictionary);
    source.fail_reads_in(variable_range);

    let error = storage
        .load_dictionaries(ALL_DICTIONARIES, &token)
        .expect_err("the variable dictionary read fails");
    assert!(matches!(error, StorageError::Io { .. }), "{error:?}");
    assert_eq!(error.kind(), ErrorKind::Io);
    assert_eq!(
        storage
            .dictionaries()
            .variable
            .get()
            .map(|dictionary| dictionary.len()),
        None
    );

    source.fail_reads_in(None);
    storage.load_dictionaries(ALL_DICTIONARIES, &token)?;
    let variables = storage
        .dictionaries()
        .variable
        .get()
        .expect("the retry loads the dictionary");
    assert_eq!(variables.len(), 62);

    let num_reads = source.num_reads.load(Ordering::Relaxed);
    storage.load_dictionaries(ALL_DICTIONARIES, &token)?;
    assert_eq!(
        source.num_reads.load(Ordering::Relaxed),
        num_reads,
        "loaded dictionaries are not read again"
    );
    Ok(())
}

#[test]
fn oversized_streams_exceed_the_reader_limits() -> anyhow::Result<()> {
    let bytes = read_bytes(&fixtures_dir().join("archives/logs.ts.clps"));
    let options = OpenOptions {
        limits: ReaderLimits {
            max_packed_stream_size: 1024,
            ..ReaderLimits::default()
        },
        ..OpenOptions::default()
    };
    let storage = ArchiveStorage::open(
        ResolvedSource::Memory(MemorySource::new(bytes)),
        &options,
        &CancellationToken::new(),
    )?;
    let read = &full_plan(&storage)[0];
    let error = read_stream(&storage, read, &StreamReadOptions::default())
        .expect_err("the stream exceeds 1 KiB");
    assert!(
        matches!(
            error,
            StorageError::Format(FormatError::LimitExceeded { .. })
        ),
        "{error:?}"
    );
    assert_eq!(error.kind(), ErrorKind::ResourceExhausted);
    Ok(())
}

#[test]
fn corrupt_streams_are_typed_errors_under_every_policy() -> anyhow::Result<()> {
    let path = fixtures_dir().join("archives/logs.ts.clps");
    let mut bytes = read_bytes(&path).to_vec();
    let storage = open_memory("logs.ts.clps");
    let read = full_plan(&storage)[0].clone();
    let frame_begin = usize::try_from(read.stream.compressed_range.begin)?;
    for byte in &mut bytes[frame_begin..frame_begin + 4] {
        *byte ^= 0x5a;
    }
    let bytes = SharedBytes::from(bytes);
    let sources = [
        ResolvedSource::Memory(MemorySource::new(bytes.clone())),
        ResolvedSource::Dyn(Arc::new(HostSource::new(bytes.clone(), false))),
        ResolvedSource::Dyn(Arc::new(HostSource::new(bytes, true))),
    ];
    for source in sources {
        let storage =
            ArchiveStorage::open(source, &OpenOptions::default(), &CancellationToken::new())?;
        let options = StreamReadOptions {
            chunk_size: 512,
            cancellation_token: CancellationToken::new(),
        };
        let error = read_stream(&storage, &read, &options).expect_err("the frame is corrupt");
        assert_eq!(error.kind(), ErrorKind::CorruptArchive, "{error:?}");
    }
    Ok(())
}

/// # Returns
///
/// The directory of large artifacts: `$PROTO_DATA_ROOT`, else the workspace's `target/proto-data`.
fn data_root() -> PathBuf {
    std::env::var_os("PROTO_DATA_ROOT").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/proto-data"),
        PathBuf::from,
    )
}

#[test]
#[ignore = "reads the large dataset archives listed in tests/queries/datasets.toml"]
fn dataset_archives_open_and_decode() -> anyhow::Result<()> {
    let manifest = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/queries/datasets.toml"),
    )?;
    let value_of = |block: &str, key: &str| -> Option<String> {
        block.lines().find_map(|line| {
            let (line_key, value) = line.split_once(" = ")?;
            (line_key == key).then(|| value.trim_matches('"').to_owned())
        })
    };
    let token = CancellationToken::new();
    let mut num_datasets = 0;
    for block in manifest.split("[[dataset]]").skip(1) {
        let archive =
            data_root().join(value_of(block, "archive").expect("datasets name an archive"));
        if !archive.exists() {
            continue;
        }
        num_datasets += 1;
        let storage = ArchiveStorage::open(
            ResolvedSource::from_path(&archive)?,
            &OpenOptions::default(),
            &token,
        )?;
        storage.load_dictionaries(ALL_DICTIONARIES, &token)?;
        let index = storage.index();
        assert_eq!(
            Some(index.tables().num_log_events().to_string()),
            value_of(block, "x_records"),
            "{}",
            archive.display()
        );
        let range = index
            .metadata()
            .timestamp_index()
            .authoritative()
            .and_then(|entry| entry.range);
        let expected_range = value_of(block, "begin_timestamp")
            .zip(value_of(block, "end_timestamp"))
            .map(|(begin, end)| clp_s_format::TimestampRange::EpochMillis {
                begin: begin.parse().expect("a number"),
                end: end.parse().expect("a number"),
            });
        assert_eq!(range, expected_range, "{}", archive.display());

        let options = StreamReadOptions {
            chunk_size: 1 << 20,
            cancellation_token: token.clone(),
        };
        for read in full_plan(&storage) {
            let decoded = read_stream(&storage, &read, &options)?;
            for ordinal in &read.tables {
                let info = *index
                    .tables()
                    .table(*ordinal)
                    .expect("planned tables exist");
                TableBytes::new(&decoded, info)?;
            }
        }
    }
    assert!(num_datasets > 0, "no dataset archive exists");
    Ok(())
}
