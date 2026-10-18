//! Error paths on mutated archives and table bytes: semantic layout errors, column tiling and
//! variable ranges, and no panic under byte mutations of every table.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use anyhow::Context;
use clp_s_format::NodeId;
use clp_s_format::NodeType;
use clp_s_format::SectionId;
use clp_s_format::SharedBytes;
use clp_s_format::TableBytes;
use clp_s_format::TableInfo;
use clp_s_resources::ErrorKind;
use clp_s_table::ColumnDemand;
use clp_s_table::ColumnEncoding;
use clp_s_table::ColumnSet;
use clp_s_table::LayoutError;
use clp_s_table::RecordVisitor;
use clp_s_table::StringRef;
use clp_s_table::Table;
use clp_s_table::TableError;
use clp_s_table::TableLayout;
use clp_s_table::ValueRef;
use clp_s_table::scan_array;

use crate::json::Json;
use crate::support::ArchiveParts;
use crate::support::OpenedArchive;
use crate::support::fixture;
use crate::support::full_demand;
use crate::support::oracle_records;
use crate::support::scan_all;
use crate::support::to_usize;

/// # Returns
///
/// The bytes of the archive file of fixture `name`.
fn fixture_bytes(name: &str) -> SharedBytes {
    SharedBytes::from(std::fs::read(fixture(name)).expect("the fixture is readable"))
}

/// # Returns
///
/// Table bytes holding `bytes` for a table described by `info` with `num_rows` rows.
fn table_bytes(bytes: Vec<u8>, info: TableInfo, num_rows: u32) -> TableBytes {
    let info = TableInfo {
        stream_offset: 0,
        uncompressed_size: bytes.len() as u64,
        num_rows,
        ..info
    };
    TableBytes::new(&SharedBytes::from(bytes), info).expect("the table fills its stream")
}

/// # Returns
///
/// The byte offset of the first column with `encoding` inside the ERT `bytes` of `layout`, which
/// has `num_rows` rows.
fn column_offset(
    bytes: &[u8],
    layout: &TableLayout,
    num_rows: usize,
    encoding: ColumnEncoding,
) -> usize {
    let mut offset = 0;
    for spec in layout.columns() {
        if encoding == spec.encoding {
            return offset;
        }
        offset += match spec.encoding {
            ColumnEncoding::Boolean => num_rows,
            ColumnEncoding::FormattedFloat64 => num_rows * 10,
            ColumnEncoding::Timestamp => num_rows * 16,
            ColumnEncoding::ClpString | ColumnEncoding::UnstructuredArray => {
                let count_offset = offset + num_rows * 8;
                let num_variables = u64::from_le_bytes(
                    bytes[count_offset..count_offset + 8]
                        .try_into()
                        .expect("eight bytes"),
                );
                num_rows * 8 + 8 + to_usize(num_variables) * 8
            }
            _ => num_rows * 8,
        };
    }
    panic!("the layout has a {encoding:?} column");
}

/// Rewrites the decoded schema map of `parts` with `mutate` applied to the node ids of the first
/// schema that has at least two nodes.
///
/// # Returns
///
/// The id of the mutated schema.
fn mutate_first_schema(parts: &mut ArchiveParts, mutate: impl FnOnce(&mut [i32])) -> i32 {
    let mut decoded = parts.decoded_section(SectionId::SchemaMap);
    let read_u32 = |bytes: &[u8], at: usize| {
        u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"))
    };
    let num_schemas = u64::from_le_bytes(decoded[..8].try_into().expect("eight bytes"));
    let mut position = 8;
    for _ in 0..num_schemas {
        let schema_id = read_u32(&decoded, position).cast_signed();
        let num_nodes = read_u32(&decoded, position + 4) as usize;
        let nodes_begin = position + 12;
        position = nodes_begin + num_nodes * 4;
        if num_nodes < 2 {
            continue;
        }
        let mut nodes: Vec<i32> = (0..num_nodes)
            .map(|index| read_u32(&decoded, nodes_begin + index * 4).cast_signed())
            .collect();
        mutate(&mut nodes);
        for (index, node) in nodes.iter().enumerate() {
            let at = nodes_begin + index * 4;
            decoded[at..at + 4].copy_from_slice(&node.to_le_bytes());
        }
        parts.set_decoded_section(SectionId::SchemaMap, &decoded);
        return schema_id;
    }
    panic!("the archive has a schema with two nodes");
}

/// Rewrites the decoded schema tree of `parts` so that node `node` has type `node_type`.
fn retype_node(parts: &mut ArchiveParts, node: NodeId, node_type: NodeType) {
    let mut decoded = parts.decoded_section(SectionId::SchemaTree);
    let mut position = 8;
    for id in 0..=node.get() {
        let key_length = u64::from_le_bytes(
            decoded[position + 4..position + 12]
                .try_into()
                .expect("eight bytes"),
        );
        position += 12 + to_usize(key_length);
        if id == node.get() {
            decoded[position] = node_type as u8;
        }
        position += 1;
    }
    parts.set_decoded_section(SectionId::SchemaTree, &decoded);
}

/// The identity of one record read by a scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RecordIdentity {
    record_id: i64,
    log_event_idx: Option<i64>,
}

/// # Returns
///
/// The identity of every record of every table of `archive`, by table, on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`OpenedArchive::load_tables`]'s return values on failure.
/// * Forwards [`scan_all`]'s return values on failure.
fn record_ids_by_table(archive: &OpenedArchive) -> anyhow::Result<Vec<Vec<RecordIdentity>>> {
    let mut tables = Vec::new();
    for table in archive.load_tables()? {
        let num_columns = table.layout().num_columns();
        let demand = Arc::new(ColumnDemand::new(
            ColumnSet::empty(num_columns),
            ColumnSet::empty(num_columns),
        ));
        let mut ids = Vec::new();
        scan_all(&table, demand, 7, |batch| {
            for row in 0..batch.num_rows() {
                ids.push(RecordIdentity {
                    record_id: batch.record_id(row)?.get(),
                    log_event_idx: batch.log_event_idx(row)?,
                });
            }
            Ok(())
        })?;
        tables.push(ids);
    }
    Ok(tables)
}

#[test]
fn retyped_log_event_idx_nodes_fail_typed_or_read_as_integers() -> anyhow::Result<()> {
    let original = OpenedArchive::open(&fixture("logs.ts"))?;
    let target = original
        .storage
        .index()
        .schema_tree()
        .log_event_idx_node()
        .context("logs.ts records log order")?;
    let expected_ids = record_ids_by_table(&original)?;
    assert!(
        expected_ids
            .iter()
            .flatten()
            .all(|identity| Some(identity.record_id) == identity.log_event_idx)
    );

    for value in 0..=u8::MAX {
        let Some(node_type) = NodeType::from_u8(value) else {
            continue;
        };
        if NodeType::DeltaInteger == node_type {
            continue;
        }
        let mut parts = ArchiveParts::from_archive(&fixture_bytes("logs.ts"));
        retype_node(&mut parts, target, node_type);
        let opened = OpenedArchive::open_bytes(parts.encode());
        if NodeType::StructuredArray == node_type {
            assert!(opened.is_err(), "structured arrays are rejected at open");
            continue;
        }
        let archive = opened?;
        if NodeType::Integer == node_type {
            let deltas: Vec<Vec<RecordIdentity>> = expected_ids
                .iter()
                .map(|ids| {
                    let mut previous = 0;
                    ids.iter()
                        .map(|identity| {
                            let delta = identity.record_id - previous;
                            previous = identity.record_id;
                            RecordIdentity {
                                record_id: delta,
                                log_event_idx: Some(delta),
                            }
                        })
                        .collect()
                })
                .collect();
            assert_eq!(record_ids_by_table(&archive)?, deltas);
            continue;
        }
        for info in archive.storage.index().tables().tables() {
            let result = archive.tables.layout(info.ordinal);
            assert!(
                matches!(
                    &result,
                    Err(LayoutError::LogEventIdxNodeType { node, node_type: actual })
                        if *node == target && *actual == node_type
                ),
                "{node_type:?}: {result:?}"
            );
            assert_eq!(
                result.map(|_| ()).map_err(|error| error.kind()),
                Err(ErrorKind::CorruptArchive)
            );
        }
    }
    Ok(())
}

#[test]
fn retyped_log_event_idx_nodes_are_a_registered_divergence_from_cpp() -> anyhow::Result<()> {
    let cpp_clp_s = std::env::var_os("CLP_S").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../../build/core/clp-s"),
        PathBuf::from,
    );
    let original = OpenedArchive::open(&fixture("logs.ts"))?;
    let target = original
        .storage
        .index()
        .schema_tree()
        .log_event_idx_node()
        .context("logs.ts records log order")?;
    let mut parts = ArchiveParts::from_archive(&fixture_bytes("logs.ts"));
    retype_node(&mut parts, target, NodeType::Float);
    let work = tempfile::tempdir()?;
    let archive_path = work.path().join("retyped.clps");
    std::fs::write(&archive_path, parts.encode())?;

    // C++ `x` never reads `log_event_idx`, so it decompresses the archive unchanged (DV-31).
    let output_dir = work.path().join("cpp");
    let output = Command::new(&cpp_clp_s)
        .arg("x")
        .arg(&archive_path)
        .arg(&output_dir)
        .output()?;
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let sorted = |records: Vec<Json>| {
        let mut serialized: Vec<String> = records.iter().map(Json::serialize).collect();
        serialized.sort();
        serialized
    };
    let cpp_records = std::fs::read(output_dir.join("original"))?
        .split(|byte| b'\n' == *byte)
        .filter(|line| !line.is_empty())
        .map(Json::parse)
        .collect::<anyhow::Result<Vec<Json>>>()?;
    assert_eq!(
        sorted(cpp_records),
        sorted(oracle_records(&fixture("logs.ts"))?)
    );

    let archive = OpenedArchive::open(&archive_path)?;
    for info in archive.storage.index().tables().tables() {
        assert_eq!(
            archive
                .tables
                .layout(info.ordinal)
                .map(|_| ())
                .map_err(|error| error.kind()),
            Err(ErrorKind::CorruptArchive)
        );
    }
    Ok(())
}

#[test]
fn schemas_out_of_node_order_fail_to_compile() -> anyhow::Result<()> {
    type Mutation = fn(&mut [i32]);
    let mutations: [(&str, Mutation); 2] = [
        ("swapped", |nodes| nodes.swap(0, 1)),
        ("duplicated", |nodes| nodes[1] = nodes[0]),
    ];
    for (description, mutation) in mutations {
        let mut parts = ArchiveParts::from_archive(&fixture_bytes("nested.default"));
        let schema_id = mutate_first_schema(&mut parts, mutation);
        let archive = OpenedArchive::open_bytes(parts.encode())?;
        let info = *archive
            .storage
            .index()
            .tables()
            .tables()
            .iter()
            .find(|info| schema_id == info.schema_id.get())
            .context("the schema has a table")?;
        for attempt in 0..2 {
            let result = archive.tables.layout(info.ordinal);
            assert!(
                matches!(
                    &result,
                    Err(LayoutError::NodeOrder { schema, .. }) if *schema == info.schema_id
                ),
                "{description}, attempt {attempt}: {result:?}"
            );
            assert_eq!(
                result.map(|_| ()).map_err(|error| error.kind()),
                Err(ErrorKind::CorruptArchive)
            );
        }
        let bytes = archive
            .table_bytes()?
            .into_iter()
            .find(|bytes| bytes.info().ordinal == info.ordinal)
            .context("the table decodes")?;
        let result = Table::load(Arc::clone(&archive.tables), bytes);
        assert!(
            matches!(
                result,
                Err(TableError::Layout(LayoutError::NodeOrder { .. }))
            ),
            "{description}: {result:?}"
        );
    }
    Ok(())
}

#[test]
fn deprecated_date_string_nodes_are_unsupported() -> anyhow::Result<()> {
    let original = OpenedArchive::open(&fixture("scalars.default"))?;
    let index = original.storage.index();
    let info = *index.tables().tables().first().context("a table")?;
    let schema = index
        .schema_map()
        .schema(info.schema_id)
        .context("the schema exists")?;
    let target = *schema
        .nodes
        .iter()
        .find(|node| {
            index
                .schema_tree()
                .node(**node)
                .is_some_and(|view| NodeType::Integer == view.node_type)
        })
        .context("the schema has an integer node")?;

    let mut parts = ArchiveParts::from_archive(&fixture_bytes("scalars.default"));
    retype_node(&mut parts, target, NodeType::DeprecatedDateString);
    let archive = OpenedArchive::open_bytes(parts.encode())?;
    let result = archive.tables.layout(info.ordinal);
    assert!(
        matches!(
            &result,
            Err(LayoutError::UnsupportedNodeType {
                node: Some(node),
                node_type: NodeType::DeprecatedDateString,
            }) if *node == target
        ),
        "{result:?}"
    );
    assert_eq!(
        result.map(|_| ()).map_err(|error| error.kind()),
        Err(ErrorKind::UnsupportedArchive)
    );
    Ok(())
}

#[test]
fn table_bytes_that_do_not_tile_fail_to_load() -> anyhow::Result<()> {
    let archive = OpenedArchive::open(&fixture("logs.ts"))?;
    let bytes = archive
        .table_bytes()?
        .into_iter()
        .max_by_key(|bytes| bytes.info().num_rows)
        .context("a table")?;
    let info = *bytes.info();
    let raw = bytes.as_bytes().to_vec();
    let mut longer = raw.clone();
    longer.push(0);
    let cases = [
        (
            "one byte short",
            raw[..raw.len() - 1].to_vec(),
            info.num_rows,
        ),
        ("one byte long", longer, info.num_rows),
        ("one row more", raw.clone(), info.num_rows + 1),
        ("one row less", raw.clone(), info.num_rows - 1),
        ("no rows", raw, 0),
    ];
    for (description, raw, num_rows) in cases {
        let result = Table::load(
            Arc::clone(&archive.tables),
            table_bytes(raw, info, num_rows),
        );
        assert!(
            matches!(&result, Err(TableError::ColumnTiling(table)) if *table == info.ordinal),
            "{description}: {result:?}"
        );
        assert_eq!(
            result.map(|_| ()).map_err(|error| error.kind()),
            Err(ErrorKind::CorruptArchive)
        );
    }
    Ok(())
}

#[test]
fn variable_offsets_out_of_range_are_rejected() -> anyhow::Result<()> {
    let archive = OpenedArchive::open(&fixture("logs.ts"))?;
    let bytes = archive
        .table_bytes()?
        .into_iter()
        .max_by_key(|bytes| bytes.info().num_rows)
        .context("a table")?;
    let info = *bytes.info();
    let table = Table::load(Arc::clone(&archive.tables), bytes.clone())?;
    let layout = Arc::clone(table.layout());
    let num_rows = to_usize(info.num_rows.into());
    let offset = column_offset(
        bytes.as_bytes(),
        &layout,
        num_rows,
        ColumnEncoding::ClpString,
    );
    let num_variables = u64::from_le_bytes(
        bytes.as_bytes()[offset + num_rows * 8..offset + num_rows * 8 + 8].try_into()?,
    );

    let table = Arc::new(table);
    let mut row_with_variables = None;
    let clp_string_column = layout
        .columns()
        .iter()
        .position(|spec| ColumnEncoding::ClpString == spec.encoding)
        .context("a ClpString column")?;
    scan_all(&table, full_demand(&table), 4096, |batch| {
        let column = batch.column(clp_s_table::ColumnIndex::new(u32::try_from(
            clp_string_column,
        )?))?;
        let clp_s_table::ColumnBatch::ClpString(text) = column else {
            anyhow::bail!("a ClpString column");
        };
        for row in 0..batch.num_rows() {
            if row_with_variables.is_none() && !text.variables(row)?.is_empty() {
                row_with_variables = Some(batch.first_row() + row);
            }
        }
        Ok(())
    })?;
    let row = row_with_variables.context("a row with variables")?;

    let patch = |variable_offset: u64| {
        let mut raw = bytes.as_bytes().to_vec();
        let at = offset + to_usize(row.into()) * 8;
        let word = u64::from_le_bytes(raw[at..at + 8].try_into().expect("eight bytes"));
        let word = (word & ((1 << 24) - 1)) | variable_offset << 24;
        raw[at..at + 8].copy_from_slice(&word.to_le_bytes());
        table_bytes(raw, info, info.num_rows)
    };

    let result = Table::load(Arc::clone(&archive.tables), patch(num_variables + 1));
    assert!(
        matches!(
            &result,
            Err(TableError::VariableOutOfRange { table, row: failed_row })
                if *table == info.ordinal && *failed_row == row
        ),
        "{result:?}"
    );

    let table = Arc::new(Table::load(
        Arc::clone(&archive.tables),
        patch(num_variables),
    )?);
    let timestamps = archive.storage.index().metadata().timestamp_index();
    let mut failures = Vec::new();
    scan_all(&table, full_demand(&table), 4096, |batch| {
        for window_row in 0..batch.num_rows() {
            if let Err(error) = crate::support::Renderer::render(&batch, window_row, timestamps) {
                failures.push((batch.first_row() + window_row, error));
            }
        }
        Ok(())
    })?;
    assert_eq!(failures.len(), 1, "{failures:?}");
    let (failed_row, error) = &failures[0];
    assert_eq!(*failed_row, row);
    let error = error
        .downcast_ref::<TableError>()
        .context("a table error")?;
    assert!(
        matches!(
            error,
            TableError::VariableOutOfRange { table, row: error_row }
                if *table == info.ordinal && *error_row == row
        ),
        "{error:?}"
    );
    Ok(())
}

/// A visitor that resolves and decodes every value, and scans every array, without assuming the
/// values are well formed.
struct Touch {
    text: Vec<u8>,
}

impl RecordVisitor for Touch {
    type Error = TableError;

    fn begin_object(&mut self, _key: Option<&[u8]>) -> Result<(), Self::Error> {
        Ok(())
    }

    fn end_object(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn value(&mut self, _key: &[u8], value: ValueRef<'_>) -> Result<(), Self::Error> {
        self.text.clear();
        match value {
            ValueRef::String(StringRef::Encoded(encoded)) => encoded.decode(&mut self.text),
            ValueRef::UnstructuredArray(encoded) => {
                encoded.decode(&mut self.text)?;
                scan_array(&self.text, |_| true)
            }
            _ => Ok(()),
        }
    }
}

/// Loads `raw` as the bytes of table `info` and walks every record, reporting whether decoding
/// succeeded; it must never panic.
fn load_and_walk(archive: &OpenedArchive, raw: Vec<u8>, info: TableInfo, num_rows: u32) {
    let Ok(table) = Table::load(
        Arc::clone(&archive.tables),
        table_bytes(raw, info, num_rows),
    ) else {
        return;
    };
    let table = Arc::new(table);
    let mut touch = Touch { text: Vec::new() };
    let _ = scan_all(&table, full_demand(&table), 64, |batch| {
        for row in 0..batch.num_rows() {
            let _ = batch.record(row).walk(&mut touch);
            let _ = batch.record_id(row);
            let _ = batch.record_timestamp_ns(row);
        }
        Ok(())
    });
}

#[test]
fn mutated_tables_never_panic() -> anyhow::Result<()> {
    for name in [
        "logs.ts",
        "arrays.default",
        "strings.default",
        "floats.default",
        "nested.default",
        "timestamps-string.ts",
        "polymorphic.default",
    ] {
        let archive = OpenedArchive::open(&fixture(name))?;
        for bytes in archive.table_bytes()? {
            let info = *bytes.info();
            let raw = bytes.as_bytes();
            let stride = raw.len() / 97 + 1;
            for position in (0..raw.len()).step_by(stride) {
                for mask in [0x01, 0x80, 0xff] {
                    let mut mutated = raw.to_vec();
                    mutated[position] ^= mask;
                    load_and_walk(&archive, mutated, info, info.num_rows);
                }
            }
            for num_rows in [0, 1, info.num_rows / 2, info.num_rows + 1, u32::MAX] {
                load_and_walk(&archive, raw.to_vec(), info, num_rows);
            }
        }
    }
    Ok(())
}

#[test]
fn authoritative_timestamps_of_non_timestamp_nodes_are_ignored() -> anyhow::Result<()> {
    const TIMESTAMP_DICTIONARY_PACKET_TYPE: u8 = 2;

    let original = OpenedArchive::open(&fixture("timestamps-int.ts"))?;
    let index = original.storage.index();
    let (info, target) = index
        .tables()
        .tables()
        .iter()
        .find_map(|info| {
            let schema = index.schema_map().schema(info.schema_id)?;
            let node = schema.nodes.iter().copied().find(|node| {
                index
                    .schema_tree()
                    .node(*node)
                    .is_some_and(|view| NodeType::Integer == view.node_type)
            })?;
            Some((*info, node))
        })
        .context("a table with an integer column")?;

    let mut parts = ArchiveParts::from_archive(&fixture_bytes("timestamps-int.ts"));
    let packet = parts.packet_mut(TIMESTAMP_DICTIONARY_PACKET_TYPE);
    let key_length = to_usize(u64::from_le_bytes(packet[8..16].try_into()?));
    let node_id_offset = 16 + key_length + 8;
    packet[node_id_offset..node_id_offset + 4]
        .copy_from_slice(&i32::try_from(target.get())?.to_le_bytes());
    let archive = OpenedArchive::open_bytes(parts.encode())?;
    let authoritative = archive
        .storage
        .index()
        .metadata()
        .timestamp_index()
        .authoritative()
        .context("an authoritative timestamp")?;
    assert_eq!(authoritative.node_id, target);

    let table = archive
        .load_tables()?
        .into_iter()
        .find(|table| table.info().ordinal == info.ordinal)
        .context("the table loads")?;
    assert_eq!(table.layout().record_timestamp_column(), None);
    assert_eq!(table.layout().columns_for_node(target).len(), 1);
    scan_all(&table, full_demand(&table), 4096, |batch| {
        for row in 0..batch.num_rows() {
            assert_eq!(batch.record_timestamp_ns(row)?, None);
        }
        Ok(())
    })?;
    Ok(())
}
