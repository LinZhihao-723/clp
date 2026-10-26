//! Every fixture archive read in full and compared with `tests/data/fixture_counts.tsv`, an
//! independent reading of the archives that follows the C++ reader (see
//! `tests/data/gen_fixture_counts.py`).

use std::collections::BTreeSet;
use std::path::Path;

use clp_s_format::DictionaryKind;
use clp_s_format::DictionaryNeeds;
use clp_s_format::FormatError;
use clp_s_format::NodeType;
use clp_s_format::OpenOptions;
use clp_s_format::SectionId;
use clp_s_format::StreamDecoder;
use clp_s_format::TableOrdinal;
use clp_s_format::TableSet;
use clp_s_format::TimestampRange;
use clp_s_format::UnsupportedFeature;
use clp_s_resources::BufferPool;
use clp_s_resources::BufferPoolConfig;
use clp_s_resources::ErrorKind;
use clp_s_resources::MemoryBudget;

use crate::support;

/// One row of `fixture_counts.tsv`.
#[derive(Debug, PartialEq, Eq)]
struct Counts {
    version: String,
    num_nodes: usize,
    num_schemas: usize,
    num_streams: usize,
    num_tables: usize,
    num_log_events: u64,
    decoded_stream_size: u64,
    num_variables: u64,
    num_logtypes: u64,
    num_arrays: u64,
    num_timestamp_entries: usize,
    num_timestamp_patterns: usize,
    timestamp_range_ms: Option<(i64, i64)>,
    num_range_entries: usize,
    has_log_order: bool,
}

/// # Returns
///
/// The expected counts of every fixture, keyed by file name.
fn expected_counts() -> Vec<(String, Counts)> {
    let text = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/fixture_counts.tsv"),
    )
    .expect("the counts file is readable");
    let mut lines = text.lines().filter(|line| !line.starts_with('#'));
    let header: Vec<&str> = lines
        .next()
        .expect("the counts file has a header")
        .split('\t')
        .collect();
    lines
        .map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            assert_eq!(fields.len(), header.len(), "malformed counts line `{line}`");
            let field = |name: &str| {
                let index = header
                    .iter()
                    .position(|column| *column == name)
                    .expect("the column exists");
                fields[index]
            };
            let number =
                |name: &str| -> u64 { field(name).parse().expect("the field is a number") };
            let count = |name: &str| support::to_usize(number(name));
            let timestamp_range_ms = match (field("timestamp_begin_ms"), field("timestamp_end_ms"))
            {
                ("-", "-") => None,
                (begin, end) => Some((
                    begin.parse().expect("the begin is a number"),
                    end.parse().expect("the end is a number"),
                )),
            };
            (
                field("file").to_owned(),
                Counts {
                    version: field("version").to_owned(),
                    num_nodes: count("num_nodes"),
                    num_schemas: count("num_schemas"),
                    num_streams: count("num_streams"),
                    num_tables: count("num_tables"),
                    num_log_events: number("num_log_events"),
                    decoded_stream_size: number("decoded_stream_size"),
                    num_variables: number("num_variables"),
                    num_logtypes: number("num_logtypes"),
                    num_arrays: number("num_arrays"),
                    num_timestamp_entries: count("num_timestamp_entries"),
                    num_timestamp_patterns: count("num_timestamp_patterns"),
                    timestamp_range_ms,
                    num_range_entries: count("num_range_entries"),
                    has_log_order: 1 == number("has_log_order"),
                },
            )
        })
        .collect()
}

/// # Returns
///
/// The counts the reader produces for the archive at `path`, on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`support::read_everything`]'s return values on failure.
fn actual_counts(path: &Path) -> anyhow::Result<Counts> {
    let contents =
        support::read_everything(&support::read_bytes(path), OpenOptions::default(), 4096)?;
    let index = &contents.index;
    let metadata = index.metadata();
    let timestamp_index = metadata.timestamp_index();
    let decoded_stream_size = contents
        .decoded_streams
        .iter()
        .map(|stream| stream.len() as u64)
        .sum();
    Ok(Counts {
        version: metadata.version().version.to_string(),
        num_nodes: index.schema_tree().len(),
        num_schemas: index.schema_map().len(),
        num_streams: index.tables().streams().len(),
        num_tables: index.tables().tables().len(),
        num_log_events: index.tables().num_log_events(),
        decoded_stream_size,
        num_variables: contents.variables.len(),
        num_logtypes: contents.logtypes.len(),
        num_arrays: contents.arrays.len(),
        num_timestamp_entries: timestamp_index.entries().len(),
        num_timestamp_patterns: timestamp_index.pattern_ids().len(),
        timestamp_range_ms: timestamp_index
            .authoritative()
            .and_then(|entry| match entry.range {
                Some(TimestampRange::EpochMillis { begin, end }) => Some((begin, end)),
                _ => None,
            }),
        num_range_entries: metadata
            .range_index()
            .map_or(0, |range_index| range_index.entries().len()),
        has_log_order: index.has_log_order(),
    })
}

#[test]
fn every_fixture_matches_the_cpp_reading() {
    let expected = expected_counts();
    let expected_files: BTreeSet<&str> = expected.iter().map(|(file, _)| file.as_str()).collect();
    let actual_files: BTreeSet<String> = support::readable_archives()
        .iter()
        .map(|path| {
            path.file_name()
                .expect("archives have names")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        expected_files,
        actual_files.iter().map(String::as_str).collect(),
        "the counts file covers exactly the readable fixtures"
    );

    let mut mismatches = Vec::new();
    for (file, expected_counts) in &expected {
        let path = support::fixtures_dir().join("archives").join(file);
        match actual_counts(&path) {
            Ok(actual) if actual == *expected_counts => {}
            Ok(actual) => mismatches.push(format!(
                "{file}: expected {expected_counts:?}, got {actual:?}"
            )),
            Err(error) => mismatches.push(format!("{file}: {error}")),
        }
    }
    assert_eq!(mismatches, Vec::<String>::new());
}

#[test]
fn tables_tile_their_decoded_streams() -> anyhow::Result<()> {
    for path in support::readable_archives() {
        let contents =
            support::read_everything(&support::read_bytes(&path), OpenOptions::default(), 1 << 20)?;
        let directory = contents.index.tables();
        assert_eq!(
            contents.tables.len(),
            directory.tables().len(),
            "{}",
            path.display()
        );
        let mut next_offsets = vec![0_u64; directory.streams().len()];
        let mut next_record_ordinal = 0;
        for table in &contents.tables {
            let info = table.info();
            let stream_index = support::to_usize(info.stream_id.get().into());
            assert_eq!(
                info.stream_offset,
                next_offsets[stream_index],
                "{}",
                path.display()
            );
            assert_eq!(table.as_bytes().len() as u64, info.uncompressed_size);
            assert_eq!(info.first_record_ordinal, next_record_ordinal);
            next_offsets[stream_index] += info.uncompressed_size;
            next_record_ordinal += u64::from(info.num_rows);
        }
        for (stream, (decoded, end)) in directory
            .streams()
            .iter()
            .zip(contents.decoded_streams.iter().zip(next_offsets))
        {
            assert_eq!(decoded.len() as u64, stream.uncompressed_size);
            assert_eq!(end, stream.uncompressed_size, "{}", path.display());
        }
    }
    Ok(())
}

#[test]
fn prefix_plans_decode_only_up_to_the_last_needed_table() -> anyhow::Result<()> {
    let path = support::fixtures_dir().join("archives/many-schemas.tables256.clps");
    let bytes = support::read_bytes(&path);
    let full = support::read_everything(&bytes, OpenOptions::default(), 1 << 20)?;
    let directory = full.index.tables();
    let num_tables = u32::try_from(directory.tables().len())?;

    let mut first_tables = TableSet::empty(num_tables);
    let mut previous_stream = None;
    for table in directory.tables() {
        if previous_stream != Some(table.stream_id) {
            first_tables.insert(table.ordinal);
            previous_stream = Some(table.stream_id);
        }
    }
    let plan = full
        .index
        .plan_reads(&first_tables, DictionaryNeeds::default());
    assert_eq!(plan.streams.len(), directory.streams().len());
    assert_eq!(plan.dictionary_ranges, std::collections::HashMap::new());

    let pool = BufferPool::new(BufferPoolConfig::default());
    let budget = MemoryBudget::new(None);
    let mut decoder = StreamDecoder::new()?;
    let mut num_partial_streams = 0;
    for read in &plan.streams {
        let first = directory
            .table(read.tables[0])
            .expect("planned tables exist");
        assert_eq!(read.tables.len(), 1);
        assert_eq!(read.decoded_prefix_size, first.uncompressed_size);
        if read.decoded_prefix_size < read.stream.uncompressed_size {
            num_partial_streams += 1;
        }
        let range = read.stream.compressed_range;
        let frame = bytes.slice(support::to_usize(range.begin)..support::to_usize(range.end));
        let full_stream = &full.decoded_streams[support::to_usize(read.stream.id.get().into())];
        let expected_prefix = &full_stream[..support::to_usize(read.decoded_prefix_size)];

        let buffer = pool.acquire(
            read.stream.uncompressed_size,
            budget.try_reserve(read.stream.uncompressed_size)?,
        )?;
        assert_eq!(
            &decoder.decode_resident(&frame, read, buffer)?[..],
            expected_prefix
        );

        let mut session = decoder.begin(
            read,
            pool.acquire(
                read.decoded_prefix_size,
                budget.try_reserve(read.decoded_prefix_size)?,
            )?,
        )?;
        for chunk in frame.chunks(64) {
            if session.push_chunk(&mut decoder, chunk)? {
                break;
            }
        }
        assert_eq!(&session.finish()?[..], expected_prefix);
    }
    assert!(
        num_partial_streams > 0,
        "the fixture has multi-table streams"
    );
    Ok(())
}

#[test]
fn a_one_byte_prefix_fetch_opens_identically() -> anyhow::Result<()> {
    let tiny_prefix = OpenOptions {
        prefix_fetch_size: 1,
        ..OpenOptions::default()
    };
    for path in support::readable_archives() {
        let bytes = support::read_bytes(&path);
        let default = support::open_index(&bytes, OpenOptions::default())?;
        let tiny = support::open_index(&bytes, tiny_prefix)?;
        assert_eq!(
            default.tables().tables(),
            tiny.tables().tables(),
            "{}",
            path.display()
        );
        assert_eq!(default.schema_tree().len(), tiny.schema_tree().len());
    }
    Ok(())
}

#[test]
fn every_stored_timestamp_pattern_compiles() -> anyhow::Result<()> {
    let mut num_patterns = 0;
    for path in support::readable_archives() {
        let index = support::open_index(&support::read_bytes(&path), OpenOptions::default())?;
        let timestamp_index = index.metadata().timestamp_index();
        for id in timestamp_index.pattern_ids() {
            timestamp_index.pattern(id)?;
            num_patterns += 1;
        }
        if let Some(authoritative) = timestamp_index.authoritative() {
            let node = index
                .schema_tree()
                .node(authoritative.node_id)
                .expect("the authoritative timestamp node exists");
            assert_eq!(node.node_type, NodeType::Timestamp, "{}", path.display());
        }
    }
    assert!(num_patterns > 0, "the corpus has timestamp patterns");
    Ok(())
}

#[test]
fn log_event_indices_appear_in_every_table_of_ordered_archives() -> anyhow::Result<()> {
    for path in support::readable_archives() {
        let index = support::open_index(&support::read_bytes(&path), OpenOptions::default())?;
        let Some(node) = index.schema_tree().log_event_idx_node() else {
            continue;
        };
        let expected: Vec<TableOrdinal> = index
            .tables()
            .tables()
            .iter()
            .map(|table| table.ordinal)
            .collect();
        assert_eq!(
            index.node_tables().tables_for_node(node),
            expected.as_slice(),
            "{}",
            path.display()
        );
    }
    Ok(())
}

#[test]
fn the_structured_array_fixture_is_rejected_at_open() {
    let path = support::fixtures_dir().join("archives/reject/arrays.structurize-arrays.clps");
    let error = support::open_index(&support::read_bytes(&path), OpenOptions::default())
        .expect_err("structured arrays are out of scope");
    assert!(
        matches!(
            error,
            FormatError::UnsupportedFeature {
                feature: UnsupportedFeature::StructuredArrays,
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(error.kind(), ErrorKind::UnsupportedArchive);
}

#[test]
fn plans_carry_the_requested_dictionary_ranges() -> anyhow::Result<()> {
    let path = support::fixtures_dir().join("archives/search.ts.clps");
    let index = support::open_index(&support::read_bytes(&path), OpenOptions::default())?;
    let sections = index.metadata().sections();
    let plan = index.plan_reads(
        &TableSet::empty(u32::try_from(index.tables().tables().len())?),
        DictionaryNeeds {
            is_variable_needed: true,
            is_logtype_needed: false,
            is_array_needed: true,
        },
    );
    assert_eq!(plan.streams, vec![]);
    assert_eq!(plan.dictionary_ranges.len(), 2);
    assert_eq!(
        plan.dictionary_ranges
            .get(&DictionaryKind::Variable)
            .copied(),
        sections.range(SectionId::VariableDictionary)
    );
    assert_eq!(
        plan.dictionary_ranges.get(&DictionaryKind::Array).copied(),
        sections.range(SectionId::ArrayDictionary)
    );
    Ok(())
}
