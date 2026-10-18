//! Compiled table filters against the reference evaluator.
//!
//! For every readable fixture archive and every query of the differential, each residual table of
//! the plan is compiled and scanned window by window (with the default window size and a small odd
//! one, so tails and window boundaries are exercised). The rows the filter selects must be exactly
//! the rows the reference evaluator matches, and exactly the rows the scalar residual evaluator of
//! the binding contract selects (which also covers records the reference cannot see as stored).
//!
//! The dataset variant (`--ignored`, best in release) checks the selected counts against the C++
//! oracle counts (or the registered Rust counts of divergent queries) and prints the evaluation
//! time of every query.

use std::fmt::Write;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use clp_s_format::NodeType;
use clp_s_format::TableOrdinal;
use clp_s_query::Query;
use clp_s_query::QueryOptions;
use clp_s_query::TimeBounds;
use clp_s_search::ArchiveSearchPlan;
use clp_s_search::CompiledTableFilter;
use clp_s_search::EvalError;
use clp_s_search::EvaluationScratch;
use clp_s_search::RecordProjection;
use clp_s_search::ResidualFilter;
use clp_s_search::TableFilterPlan;
use clp_s_table::ColumnDemand;
use clp_s_table::ColumnSet;
use clp_s_table::Table;
use clp_s_table::TableScanner;

use crate::differential::all_queries;
use crate::differential::prepare;
use crate::support::ArchiveRows;
use crate::support::OpenedArchive;
use crate::support::Planned;
use crate::support::ResidualEvaluator;
use crate::support::data_root;
use crate::support::fixture;
use crate::support::oracle_counts;
use crate::support::plan;
use crate::support::readable_archives;
use crate::support::stem;
use crate::support::suite_queries;

/// The window sizes every residual table is scanned with.
const WINDOW_SIZES: [u32; 2] = [4096, 37];

/// Counts over the whole differential.
#[derive(Default)]
struct Tally {
    compiled_tables: u64,
    rows_checked: u64,
    unverifiable_rows: u64,
    selected_rows: u64,
}

/// Scans `table` with the compiled `filter` in windows of `window_size` rows.
///
/// # Returns
///
/// The selected rows of the table, in ascending order, on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`TableScanner::next_window`]'s return values on failure.
/// * Forwards [`CompiledTableFilter::evaluate`]'s return values on failure.
pub fn selected_rows(
    table: &Arc<Table>,
    filter: &CompiledTableFilter,
    window_size: u32,
    scratch: &mut EvaluationScratch,
) -> anyhow::Result<Vec<u32>> {
    let num_columns = table.layout().num_columns();
    let demand = Arc::new(ColumnDemand::new(
        filter.required_columns().clone(),
        ColumnSet::empty(num_columns),
    ));
    let mut scanner = TableScanner::new(NonZeroU32::new(window_size).context("non-zero")?);
    let mut state = scanner.start(Arc::clone(table), demand);
    let mut rows = Vec::new();
    while let Some(window) = scanner.next_window(&mut state)? {
        let first_row = window.columns.first_row();
        filter.evaluate(window, scratch)?;
        let mut window_rows = Vec::new();
        scanner
            .batch(&state)
            .selection()
            .extend_indices(&mut window_rows);
        rows.extend(window_rows.into_iter().map(|row| first_row + row));
    }
    Ok(rows)
}

/// Compiles the residual of `table` and scans it with every window size.
///
/// # Returns
///
/// The selected rows (equal for every window size, else an error) on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if two window sizes select different rows.
/// * Forwards [`CompiledTableFilter::compile`]'s return values on failure.
/// * Forwards [`selected_rows`]'s return values on failure.
fn evaluate_table(
    plan: &ArchiveSearchPlan,
    residual: &ResidualFilter,
    table: &Arc<Table>,
    scratch: &mut EvaluationScratch,
) -> anyhow::Result<Vec<u32>> {
    let filter = CompiledTableFilter::compile(plan, residual, table.layout())?;
    let mut selected: Option<Vec<u32>> = None;
    for window_size in WINDOW_SIZES {
        let rows = selected_rows(table, &filter, window_size, scratch)?;
        if let Some(previous) = &selected {
            anyhow::ensure!(
                *previous == rows,
                "windows of {window_size} rows select {rows:?}, larger ones {previous:?}"
            );
        }
        selected = Some(rows);
    }
    Ok(selected.unwrap_or_default())
}

#[test]
fn compiled_filters_agree_with_the_reference_on_every_fixture() -> anyhow::Result<()> {
    let queries = all_queries()?;
    let mut stats = Tally::default();
    let mut failures = Vec::new();
    let mut scratch = EvaluationScratch::default();
    for path in readable_archives() {
        let name = stem(&path);
        let rows = ArchiveRows::load(&path)?;
        let archive = OpenedArchive::open(&path)?;
        let tables = archive.load_tables()?;
        let tree = archive.storage.index().schema_tree();
        let has_auto_generated_keys = tree
            .subtree_root(b"@", NodeType::Object)
            .is_some_and(|root| !tree.children(root).is_empty());
        for suite_query in &queries {
            let Some((query, projection)) = prepare(suite_query, has_auto_generated_keys) else {
                continue;
            };
            let Planned::Plan(plan) = plan(&archive, &query, &projection)?.0 else {
                continue;
            };
            archive.load(plan.filter_dictionary_needs())?;
            let mut fail = |message: String| {
                failures.push(format!(
                    "{name} / {} `{}`: {message}",
                    suite_query.id, suite_query.kql
                ));
            };
            for table_plan in plan.tables() {
                let TableFilterPlan::Residual(residual) = &table_plan.filter else {
                    continue;
                };
                let ordinal = table_plan.table.get() as usize;
                let (Some(table), Some(table_rows)) =
                    (tables.get(ordinal), rows.tables.get(ordinal))
                else {
                    fail(format!("table {ordinal} does not exist"));
                    continue;
                };
                stats.compiled_tables += 1;
                let selected = match evaluate_table(&plan, residual, table, &mut scratch) {
                    Ok(selected) => selected,
                    Err(error) => {
                        fail(format!("table {ordinal}: {error:#}\n{plan}"));
                        continue;
                    }
                };
                stats.selected_rows += selected.len() as u64;
                for (row_index, row) in table_rows.iter().enumerate() {
                    let is_selected = selected.binary_search(&u32::try_from(row_index)?).is_ok();
                    let mut evaluator = ResidualEvaluator {
                        query: &query,
                        residual,
                        prefilter_violations: Vec::new(),
                    };
                    let is_contract_match = evaluator.matches(row);
                    let reference = if row.unverifiable.is_some() {
                        stats.unverifiable_rows += 1;
                        None
                    } else {
                        stats.rows_checked += 1;
                        Some(rows.reference_matches(&query, row))
                    };
                    if is_selected != is_contract_match
                        || reference.is_some_and(|is_match| is_match != is_selected)
                    {
                        fail(format!(
                            "table {ordinal} row {row_index}: the filter says {is_selected}, the \
                             binding contract {is_contract_match}, the reference {reference:?}, \
                             on {}\n{plan}",
                            row.record
                        ));
                        break;
                    }
                }
            }
        }
    }

    println!(
        "{} residual tables compiled, {} rows checked against the reference ({} unverifiable), {} \
         rows selected",
        stats.compiled_tables, stats.rows_checked, stats.unverifiable_rows, stats.selected_rows
    );
    assert!(
        stats.compiled_tables > 1_000,
        "the differential compiles enough tables"
    );
    assert!(
        stats.rows_checked > 100_000,
        "the differential covers enough rows"
    );
    if !failures.is_empty() {
        let mut report = format!("{} disagreements; the first ones:\n", failures.len());
        for failure in failures.iter().take(20) {
            let _ = writeln!(report, "  {failure}");
        }
        panic!("{report}");
    }
    Ok(())
}

/// # Returns
///
/// The plan of `kql` on `archive`, which must not be pruned.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if the archive is pruned.
/// * Forwards [`Query::from_kql`]'s return values on failure.
/// * Forwards [`plan`]'s return values on failure.
fn plan_of(archive: &OpenedArchive, kql: &str) -> anyhow::Result<Arc<ArchiveSearchPlan>> {
    let query = Arc::new(Query::from_kql(kql, &QueryOptions::default())?);
    match plan(archive, &query, &RecordProjection::Full)?.0 {
        Planned::Plan(plan) => Ok(plan),
        Planned::Pruned(report) => anyhow::bail!("{kql} is pruned: {report:?}"),
    }
}

#[test]
fn residuals_compile_only_against_their_plan_and_layout() -> anyhow::Result<()> {
    let archive = OpenedArchive::open(&fixture("search.ts"))?;
    let plan = plan_of(&archive, "n > 1")?;
    let other_plan = plan_of(&archive, "n > 2")?;
    let (table, residual) = plan
        .tables()
        .iter()
        .find_map(|table| match &table.filter {
            TableFilterPlan::Residual(residual) => Some((table.table, residual)),
            TableFilterPlan::MatchAll => None,
        })
        .context("`n > 1` has a residual table")?;
    let layout = archive.tables.layout(table)?;
    CompiledTableFilter::compile(&plan, residual, &layout)?;

    let foreign = CompiledTableFilter::compile(&other_plan, residual, &layout);
    assert!(
        matches!(foreign, Err(EvalError::PlanMismatch { .. })),
        "{foreign:?}"
    );

    let bound_nodes: Vec<_> = residual
        .bindings()
        .iter()
        .flat_map(|binding| binding.columns.iter().map(|column| column.node_id))
        .collect();
    let num_tables = u32::try_from(archive.storage.index().tables().tables().len())?;
    let mut num_mismatches = 0;
    for ordinal in 0..num_tables {
        let other_layout = archive.tables.layout(TableOrdinal::new(ordinal))?;
        let has_bound_columns = bound_nodes
            .iter()
            .all(|node| !other_layout.columns_for_node(*node).is_empty());
        if has_bound_columns {
            continue;
        }
        let result = CompiledTableFilter::compile(&plan, residual, &other_layout);
        assert!(
            matches!(result, Err(EvalError::PlanMismatch { .. })),
            "table {ordinal}: {result:?}"
        );
        num_mismatches += 1;
    }
    assert!(num_mismatches > 0, "some table lacks the bound columns");
    Ok(())
}

/// Counts the records `plan` selects in `tables`, evaluating residual tables with compiled
/// filters in default-size windows.
///
/// # Returns
///
/// The number of selected records on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if a planned table does not exist.
/// * Forwards [`CompiledTableFilter::compile`]'s return values on failure.
/// * Forwards [`selected_rows`]'s return values on failure.
fn count_selected(
    plan: &ArchiveSearchPlan,
    tables: &[Arc<Table>],
    scratch: &mut EvaluationScratch,
) -> anyhow::Result<u64> {
    let mut count = 0_u64;
    for table_plan in plan.tables() {
        match &table_plan.filter {
            TableFilterPlan::MatchAll => count += u64::from(table_plan.num_rows),
            TableFilterPlan::Residual(residual) => {
                let table = tables
                    .get(table_plan.table.get() as usize)
                    .context("planned tables exist")?;
                let filter = CompiledTableFilter::compile(plan, residual, table.layout())?;
                count += selected_rows(table, &filter, WINDOW_SIZES[0], scratch)?.len() as u64;
            }
        }
    }
    Ok(count)
}

#[test]
#[ignore = "reads the large dataset archives"]
fn dataset_filters_select_the_cpp_counts() -> anyhow::Result<()> {
    let counts = oracle_counts()?;
    let queries = suite_queries()?;
    let mut failures = Vec::new();
    let mut scratch = EvaluationScratch::default();
    for dataset in ["bench", "spider", "synthetic"] {
        let path = data_root().join("datasets").join(format!("{dataset}.clps"));
        let archive = OpenedArchive::open(&path)?;
        let start = Instant::now();
        let tables = archive.load_tables()?;
        println!(
            "{dataset}: {} tables loaded in {:.1} ms",
            tables.len(),
            start.elapsed().as_secs_f64() * 1e3
        );
        let target = format!("dataset:{dataset}");
        for suite_query in queries.iter().filter(|query| query.target == target) {
            let options =
                QueryOptions::default().with_case_sensitivity(suite_query.is_case_sensitive);
            let query = Arc::new(
                Query::from_kql(&suite_query.kql, &options)?.with_time_bounds(
                    TimeBounds::from_epoch_millis(suite_query.tge, suite_query.tle),
                ),
            );
            let start = Instant::now();
            let (planned, _) = plan(&archive, &query, &RecordProjection::Full)?;
            let planning_ms = start.elapsed().as_secs_f64() * 1e3;
            let start = Instant::now();
            let count = match &planned {
                Planned::Pruned(_) => 0,
                Planned::Plan(plan) => {
                    archive.load(plan.filter_dictionary_needs())?;
                    count_selected(plan, &tables, &mut scratch)?
                }
            };
            let evaluation_ms = start.elapsed().as_secs_f64() * 1e3;
            let expected = suite_query
                .expected_rust_count
                .or_else(|| counts.get(&suite_query.id).copied());
            println!(
                "{dataset} {}: selected {count} / expected {expected:?}; planned in \
                 {planning_ms:.2} ms, evaluated in {evaluation_ms:.2} ms",
                suite_query.id
            );
            if expected.is_some_and(|expected| expected != count) {
                failures.push(format!(
                    "{dataset} {}: selected {count}, expected {expected:?}",
                    suite_query.id
                ));
            }
        }
    }
    assert_eq!(failures, Vec::<String>::new());
    Ok(())
}
