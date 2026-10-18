//! No panic under byte mutations of every table when its records are rendered through the
//! production template path.

use std::sync::Arc;

use clp_s_format::SharedBytes;
use clp_s_format::TableBytes;
use clp_s_format::TableInfo;
use clp_s_json::JsonRecordBuffer;
use clp_s_table::ResolvedProjection;
use clp_s_table::Table;

use crate::support::EMULATED;
use crate::support::OpenedArchive;
use crate::support::SHORTEST;
use crate::support::fixture;
use crate::support::output_demand;
use crate::support::scan_selected;

/// Loads `raw` as the bytes of table `info` with `num_rows` rows and renders every record through
/// the table's template under each float option; corrupt bytes may fail but must never panic.
fn load_and_render(archive: &OpenedArchive, raw: Vec<u8>, info: TableInfo, num_rows: u32) {
    let info = TableInfo {
        stream_offset: 0,
        uncompressed_size: raw.len() as u64,
        num_rows,
        ..info
    };
    let Ok(bytes) = TableBytes::new(&SharedBytes::from(raw), info) else {
        return;
    };
    let Ok(table) = Table::load(Arc::clone(&archive.tables), bytes) else {
        return;
    };
    let table = Arc::new(table);
    let mut buffer = JsonRecordBuffer::new();
    for options in [EMULATED, SHORTEST] {
        let Ok(template) =
            archive
                .context
                .template(info.ordinal, &ResolvedProjection::Full, &options)
        else {
            continue;
        };
        let _ = scan_selected(
            &table,
            output_demand(&table, &ResolvedProjection::Full),
            64,
            |_| true,
            |batch| {
                buffer.clear();
                template
                    .bind(batch, &archive.context)?
                    .write_selected(&mut buffer)?;
                Ok(())
            },
        );
    }
}

#[test]
fn mutated_tables_never_panic_when_rendered() -> anyhow::Result<()> {
    for name in [
        "logs.ts",
        "arrays.default",
        "strings.default",
        "floats.default",
        "nested.default",
        "timestamps-string.ts",
        "polymorphic.default",
        "kvir-text.ts",
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
                    load_and_render(&archive, mutated, info, info.num_rows);
                }
            }
            for num_rows in [0, 1, info.num_rows / 2, info.num_rows + 1, u32::MAX] {
                load_and_render(&archive, raw.to_vec(), info, num_rows);
            }
        }
    }
    Ok(())
}
