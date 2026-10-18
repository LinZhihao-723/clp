//! Partial selections and chosen rows: `write_selected` renders exactly the selected rows in row
//! order, `write_rows` the given rows in the given order, both appending to what the buffer holds.

use std::sync::Arc;

use clp_s_json::JsonRecordBuffer;
use clp_s_table::DEFAULT_BATCH_NUM_ROWS;
use clp_s_table::ResolvedProjection;

use crate::support::EMULATED;
use crate::support::OpenedArchive;
use crate::support::fixture;
use crate::support::output_demand;
use crate::support::scan_selected;

/// # Returns
///
/// The records of `buffer`, as text.
fn records(buffer: &JsonRecordBuffer) -> Vec<String> {
    (0..buffer.len())
        .map(|index| {
            String::from_utf8_lossy(buffer.record(index).expect("the record exists")).into_owned()
        })
        .collect()
}

#[test]
fn partial_selections_render_only_the_selected_rows() -> anyhow::Result<()> {
    let archive = OpenedArchive::open(&fixture("logs.ts"))?;
    let projection = ResolvedProjection::Full;
    for table in archive.load_tables()? {
        let template = archive
            .context
            .template(table.info().ordinal, &projection, &EMULATED)?;
        let mut all = JsonRecordBuffer::new();
        scan_selected(
            &table,
            output_demand(&table, &projection),
            DEFAULT_BATCH_NUM_ROWS,
            |_| true,
            |batch| {
                template
                    .bind(batch, &archive.context)?
                    .write_selected(&mut all)?;
                Ok(())
            },
        )?;
        let all = records(&all);

        for batch_num_rows in [5, DEFAULT_BATCH_NUM_ROWS] {
            let mut selected = JsonRecordBuffer::new();
            let mut num_written = 0;
            scan_selected(
                &table,
                output_demand(&table, &projection),
                batch_num_rows,
                |row| 1 == row % 3,
                |batch| {
                    num_written += template
                        .bind(batch, &archive.context)?
                        .write_selected(&mut selected)?;
                    Ok(())
                },
            )?;
            let expected: Vec<String> = all.iter().skip(1).step_by(3).cloned().collect();
            assert_eq!(num_written as usize, expected.len());
            assert_eq!(records(&selected), expected, "{batch_num_rows}-row windows");
        }
    }
    Ok(())
}

#[test]
fn chosen_rows_render_in_the_given_order() -> anyhow::Result<()> {
    let archive = OpenedArchive::open(&fixture("logs.ts"))?;
    let projection = ResolvedProjection::Full;
    let table = Arc::clone(
        archive
            .load_tables()?
            .iter()
            .find(|table| table.num_rows() > 10)
            .expect("a table has more than ten rows"),
    );
    let template = archive
        .context
        .template(table.info().ordinal, &projection, &EMULATED)?;
    let mut all = JsonRecordBuffer::new();
    let mut chosen = JsonRecordBuffer::new();
    let mut is_first_window = true;
    scan_selected(
        &table,
        output_demand(&table, &projection),
        DEFAULT_BATCH_NUM_ROWS,
        |_| true,
        |batch| {
            let bound = template.bind(batch, &archive.context)?;
            bound.write_selected(&mut all)?;
            if is_first_window {
                bound.write_rows(&[7, 0, 3, 7], &mut chosen)?;
                bound.write_rows(&[], &mut chosen)?;
                is_first_window = false;
            }
            Ok(())
        },
    )?;
    let all = records(&all);
    assert_eq!(
        records(&chosen),
        vec![
            all[7].clone(),
            all[0].clone(),
            all[3].clone(),
            all[7].clone()
        ]
    );
    Ok(())
}

#[test]
fn templates_are_cached_per_schema_projection_and_options() -> anyhow::Result<()> {
    let archive = OpenedArchive::open(&fixture("many-schemas.tables256"))?;
    let index = archive.storage.index();
    let tables = index.tables().tables();
    let first = &tables[0];
    let full = archive
        .context
        .template(first.ordinal, &ResolvedProjection::Full, &EMULATED)?;
    let again = archive
        .context
        .template(first.ordinal, &ResolvedProjection::Full, &EMULATED)?;
    assert!(Arc::ptr_eq(&full, &again), "the template is reused");
    let shortest = archive.context.template(
        first.ordinal,
        &ResolvedProjection::Full,
        &crate::support::SHORTEST,
    )?;
    assert!(
        !Arc::ptr_eq(&full, &shortest),
        "options change the template"
    );
    let projected = archive.context.template(
        first.ordinal,
        &ResolvedProjection::Nodes(Box::default()),
        &EMULATED,
    )?;
    assert!(
        !Arc::ptr_eq(&full, &projected),
        "projections change the template"
    );
    if let Some(other) = tables
        .iter()
        .find(|table| table.schema_id != first.schema_id)
    {
        let other =
            archive
                .context
                .template(other.ordinal, &ResolvedProjection::Full, &EMULATED)?;
        assert!(!Arc::ptr_eq(&full, &other), "schemas change the template");
    }
    Ok(())
}
