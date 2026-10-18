//! Error paths: columns missing from the batch, corrupt dictionary ids in table bytes (the record
//! buffer rolls back), unparsable timestamp patterns (DV-32), and batches of another archive.

use std::sync::Arc;

use clp_s_format::ArchiveHeader;
use clp_s_format::FormatError;
use clp_s_format::SharedBytes;
use clp_s_format::TableBytes;
use clp_s_format::TableInfo;
use clp_s_json::JsonError;
use clp_s_json::JsonRecordBuffer;
use clp_s_resources::ErrorKind;
use clp_s_table::ColumnDemand;
use clp_s_table::ColumnEncoding;
use clp_s_table::ColumnSet;
use clp_s_table::DEFAULT_BATCH_NUM_ROWS;
use clp_s_table::ResolvedProjection;
use clp_s_table::Table;
use clp_s_table::TableError;

use crate::support::EMULATED;
use crate::support::OpenedArchive;
use crate::support::fixture;
use crate::support::output_demand;
use crate::support::scan_selected;

#[test]
fn binding_fails_when_an_output_column_is_not_decoded() -> anyhow::Result<()> {
    let archive = OpenedArchive::open(&fixture("scalars.default"))?;
    let table = Arc::clone(&archive.load_tables()?[0]);
    let template =
        archive
            .context
            .template(table.info().ordinal, &ResolvedProjection::Full, &EMULATED)?;
    let num_columns = table.layout().num_columns();
    assert!(num_columns > 0, "the table has columns");
    let demand = Arc::new(ColumnDemand::new(
        ColumnSet::empty(num_columns),
        ColumnSet::empty(num_columns),
    ));
    let mut num_failures = 0;
    scan_selected(
        &table,
        demand,
        DEFAULT_BATCH_NUM_ROWS,
        |_| true,
        |batch| {
            let result = template.bind(batch, &archive.context);
            assert!(
                matches!(
                    result,
                    Err(JsonError::Table(TableError::ColumnNotDecoded(_)))
                ),
                "{result:?}"
            );
            if let Err(error) = result {
                assert_eq!(error.kind(), ErrorKind::Internal);
            }
            num_failures += 1;
            Ok(())
        },
    )?;
    assert!(num_failures > 0, "a window was scanned");
    Ok(())
}

#[test]
fn corrupt_variable_ids_fail_and_roll_back_the_call() -> anyhow::Result<()> {
    const CORRUPT_ROW: usize = 2;

    let archive = OpenedArchive::open(&fixture("strings.default"))?;
    let (bytes, column_offset) = archive
        .table_bytes()?
        .into_iter()
        .find_map(|bytes| {
            let info = *bytes.info();
            let layout = archive.tables.layout(info.ordinal).ok()?;
            let encodings: Vec<ColumnEncoding> =
                layout.columns().iter().map(|spec| spec.encoding).collect();
            leading_variable_string_offset(&encodings, info.num_rows)
                .filter(|_| info.num_rows as usize > CORRUPT_ROW)
                .map(|offset| (bytes, offset))
        })
        .expect("a table has a VarString column after fixed-width columns only");

    let mut mutated = bytes.as_bytes().to_vec();
    let id_offset = column_offset + 8 * CORRUPT_ROW;
    mutated[id_offset..id_offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    let info = TableInfo {
        stream_offset: 0,
        ..*bytes.info()
    };
    let table = Arc::new(Table::load(
        Arc::clone(&archive.tables),
        TableBytes::new(&SharedBytes::from(mutated), info)?,
    )?);
    let template = archive
        .context
        .template(info.ordinal, &ResolvedProjection::Full, &EMULATED)?;

    let mut buffer = JsonRecordBuffer::new();
    let mut is_checked = false;
    scan_selected(
        &table,
        output_demand(&table, &ResolvedProjection::Full),
        DEFAULT_BATCH_NUM_ROWS,
        |_| true,
        |batch| {
            let bound = template.bind(batch, &archive.context)?;
            bound.write_rows(&[0], &mut buffer)?;
            let before = buffer.as_jsonl().to_vec();

            let result = bound.write_selected(&mut buffer);
            assert!(
                matches!(result, Err(JsonError::Format(FormatError::Corrupt { .. }))),
                "{result:?}"
            );
            if let Err(error) = result {
                assert_eq!(error.kind(), ErrorKind::CorruptArchive);
            }
            assert_eq!(buffer.len(), 1, "the failed call's records are removed");
            assert_eq!(buffer.as_jsonl(), before.as_slice());

            let result = bound.write_rows(&[1, 2], &mut buffer);
            assert!(result.is_err(), "{result:?}");
            assert_eq!(buffer.as_jsonl(), before.as_slice());
            is_checked = true;
            Ok(())
        },
    )?;
    assert!(is_checked, "the corrupt window was rendered");
    Ok(())
}

#[test]
fn unparsable_timestamp_patterns_fail_only_rendering() -> anyhow::Result<()> {
    let path = fixture("timestamps-string.ts");
    let original = std::fs::read(&path)?;
    let pattern = {
        let archive = OpenedArchive::open(&path)?;
        let timestamps = archive.storage.index().metadata().timestamp_index();
        let id = *timestamps
            .pattern_ids()
            .first()
            .expect("the archive has timestamp patterns");
        timestamps
            .pattern_text(id)
            .expect("the pattern exists")
            .to_vec()
    };
    let archive = OpenedArchive::open_bytes(replace_in_metadata(
        &original,
        &pattern,
        &break_first_specifier(&pattern),
    ))?;

    let mut failures = Vec::new();
    for table in archive.load_tables()? {
        let template =
            archive
                .context
                .template(table.info().ordinal, &ResolvedProjection::Full, &EMULATED)?;
        let mut buffer = JsonRecordBuffer::new();
        scan_selected(
            &table,
            output_demand(&table, &ResolvedProjection::Full),
            DEFAULT_BATCH_NUM_ROWS,
            |_| true,
            |batch| {
                if let Err(error) = template
                    .bind(batch, &archive.context)?
                    .write_selected(&mut buffer)
                {
                    failures.push(error);
                }
                Ok(())
            },
        )?;
    }
    assert!(!failures.is_empty(), "a record uses the broken pattern");
    for error in failures {
        assert!(
            matches!(
                error,
                JsonError::Format(FormatError::UnsupportedFeature { .. })
            ),
            "{error:?}"
        );
        assert_eq!(error.kind(), ErrorKind::UnsupportedArchive);
    }
    Ok(())
}

#[test]
#[should_panic(expected = "another archive")]
fn binding_a_batch_of_another_archive_panics() {
    let path = fixture("scalars.default");
    let first = OpenedArchive::open(&path).expect("the fixture opens");
    let second = OpenedArchive::open(&path).expect("the fixture opens");
    let table = Arc::clone(&first.load_tables().expect("the tables load")[0]);
    let template = second
        .context
        .template(table.info().ordinal, &ResolvedProjection::Full, &EMULATED)
        .expect("the template compiles");
    scan_selected(
        &table,
        output_demand(&table, &ResolvedProjection::Full),
        DEFAULT_BATCH_NUM_ROWS,
        |_| true,
        |batch| {
            let _ = template.bind(batch, &second.context);
            Ok(())
        },
    )
    .expect("the scan runs");
}

/// # Returns
///
/// The byte offset of the first `VariableString` column of a table of `num_rows` rows whose
/// columns have `encodings`, if only fixed-width columns precede it.
fn leading_variable_string_offset(encodings: &[ColumnEncoding], num_rows: u32) -> Option<usize> {
    let num_rows = num_rows as usize;
    let mut offset = 0;
    for encoding in encodings {
        let width = match encoding {
            ColumnEncoding::VariableString => return Some(offset),
            ColumnEncoding::Int64
            | ColumnEncoding::DeltaInt64
            | ColumnEncoding::Float64
            | ColumnEncoding::DictionaryFloat => 8,
            ColumnEncoding::FormattedFloat64 => 10,
            ColumnEncoding::Boolean => 1,
            ColumnEncoding::Timestamp => 16,
            _ => return None,
        };
        offset += width * num_rows;
    }
    None
}

/// # Returns
///
/// `pattern` with the character of its first `\` specifier replaced by `q`, which no pattern
/// specifier uses.
fn break_first_specifier(pattern: &[u8]) -> Vec<u8> {
    let mut broken = pattern.to_vec();
    let position = broken
        .windows(2)
        .position(|pair| b'\\' == pair[0] && pair[1].is_ascii_alphabetic())
        .expect("the pattern has a specifier");
    broken[position + 1] = b'q';
    broken
}

/// # Returns
///
/// The SFA `archive` with the only occurrence of `from` in its decoded metadata section replaced by
/// `to` (of the same length), recompressed, with the header's sizes updated; section offsets are
/// relative to the end of the metadata section, so they stay valid.
fn replace_in_metadata(archive: &[u8], from: &[u8], to: &[u8]) -> SharedBytes {
    assert_eq!(from.len(), to.len(), "the replacement keeps the length");
    let header_size = usize::try_from(ArchiveHeader::SIZE).expect("small header");
    let metadata_size =
        u32::from_le_bytes(archive[56..60].try_into().expect("four bytes")) as usize;
    let metadata_end = header_size + metadata_size;
    let mut metadata =
        zstd::decode_all(&archive[header_size..metadata_end]).expect("the metadata decodes");
    let positions: Vec<usize> = metadata
        .windows(from.len())
        .enumerate()
        .filter(|(_, window)| *window == from)
        .map(|(position, _)| position)
        .collect();
    assert_eq!(positions.len(), 1, "the text occurs once in the metadata");
    metadata[positions[0]..positions[0] + to.len()].copy_from_slice(to);
    let metadata = zstd::encode_all(metadata.as_slice(), 3).expect("zstd compresses");

    let mut patched = archive[..header_size].to_vec();
    patched.extend_from_slice(&metadata);
    patched.extend_from_slice(&archive[metadata_end..]);
    let compressed_size = patched.len() as u64;
    patched[16..24].copy_from_slice(&compressed_size.to_le_bytes());
    patched[56..60].copy_from_slice(
        &u32::try_from(metadata.len())
            .expect("small metadata")
            .to_le_bytes(),
    );
    SharedBytes::from(patched)
}
