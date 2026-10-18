//! Format compatibility: every table of every C++ fixture archive decodes to the records C++
//! `clp-s x` printed, compared as multisets with C++ key order kept.

use std::collections::HashMap;

use clp_s_table::DEFAULT_BATCH_NUM_ROWS;

use crate::json::Json;
use crate::support::OpenedArchive;
use crate::support::oracle_records;
use crate::support::readable_archives;
use crate::support::render_archive;
use crate::support::stem;

/// # Returns
///
/// The records of `left` missing from `right`, counting duplicates.
fn missing_from(left: &[String], right: &[String]) -> Vec<String> {
    let mut counts: HashMap<&str, i64> = HashMap::new();
    for record in right {
        *counts.entry(record).or_default() += 1;
    }
    let mut missing = Vec::new();
    for record in left {
        let count = counts.entry(record).or_default();
        if 0 == *count {
            missing.push(record.clone());
        } else {
            *count -= 1;
        }
    }
    missing
}

#[test]
fn every_fixture_decodes_to_the_cpp_records() -> anyhow::Result<()> {
    let mut failures = Vec::new();
    let mut num_records_checked = 0;
    for path in readable_archives() {
        let archive = OpenedArchive::open(&path)?;
        let expected: Vec<String> = oracle_records(&path)?.iter().map(Json::serialize).collect();
        // Small windows make every delta-encoded column cross window boundaries.
        for batch_num_rows in [1, 7, DEFAULT_BATCH_NUM_ROWS] {
            let rendered: Vec<String> = render_archive(&archive, batch_num_rows)?
                .iter()
                .map(Json::serialize)
                .collect();
            let unexpected = missing_from(&rendered, &expected);
            let missing = missing_from(&expected, &rendered);
            if rendered.len() != expected.len() || !unexpected.is_empty() || !missing.is_empty() {
                failures.push(format!(
                    "{} ({batch_num_rows}-row windows): {} records rendered, {} expected\n  \
                     unexpected: {:?}\n  missing: {:?}",
                    stem(&path),
                    rendered.len(),
                    expected.len(),
                    unexpected.iter().take(3).collect::<Vec<_>>(),
                    missing.iter().take(3).collect::<Vec<_>>(),
                ));
            }
            num_records_checked += rendered.len();
        }
    }
    assert!(
        failures.is_empty(),
        "{} fixtures differ from C++:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        num_records_checked > 3 * 10_000,
        "{num_records_checked} records"
    );
    Ok(())
}

#[test]
fn every_fixture_layout_compiles() -> anyhow::Result<()> {
    for path in readable_archives() {
        let archive = OpenedArchive::open(&path)?;
        for info in archive.storage.index().tables().tables() {
            let layout = archive.tables.layout(info.ordinal)?;
            assert_eq!(layout.schema_id(), info.schema_id, "{}", stem(&path));
        }
    }
    Ok(())
}
