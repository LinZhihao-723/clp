//! The planner on the dataset archives (large, `--ignored`, best in release): every row of every
//! table is checked as in the fixture differential, and the number of records the plan selects
//! must equal the C++ oracle count, or the registered Rust count of a divergent query.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use clp_s_query::Query;
use clp_s_query::QueryOptions;
use clp_s_query::TimeBounds;
use clp_s_search::RecordProjection;
use clp_s_search::TableFilterPlan;

use crate::support::ArchiveRows;
use crate::support::OpenedArchive;
use crate::support::Planned;
use crate::support::ResidualEvaluator;
use crate::support::SuiteQuery;
use crate::support::data_root;
use crate::support::oracle_counts;
use crate::support::plan;
use crate::support::suite_queries;

/// One planned dataset query and its tallies.
struct Checked {
    id: String,
    query: Arc<Query>,
    planned: Planned,
    expected_count: Option<u64>,
    reference_count: u64,
    plan_count: u64,
    num_disagreements: u64,
    planning_ms: f64,
}

/// Plans every suite query of `dataset` against `archive`.
///
/// # Returns
///
/// The planned queries on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`Query::from_kql`]'s return values on failure.
/// * Forwards [`plan`]'s return values on failure.
fn plan_queries(
    dataset: &str,
    archive: &OpenedArchive,
    queries: &[SuiteQuery],
    counts: &HashMap<String, u64>,
) -> anyhow::Result<Vec<Checked>> {
    let target = format!("dataset:{dataset}");
    let mut checked = Vec::new();
    for suite_query in queries.iter().filter(|query| query.target == target) {
        let options = QueryOptions::default().with_case_sensitivity(suite_query.is_case_sensitive);
        let query = Arc::new(
            Query::from_kql(&suite_query.kql, &options)?.with_time_bounds(
                TimeBounds::from_epoch_millis(suite_query.tge, suite_query.tle),
            ),
        );
        let start = Instant::now();
        let (planned, _) = plan(archive, &query, &RecordProjection::Full)?;
        checked.push(Checked {
            id: suite_query.id.clone(),
            query,
            planned,
            expected_count: suite_query
                .expected_rust_count
                .or_else(|| counts.get(&suite_query.id).copied()),
            reference_count: 0,
            plan_count: 0,
            num_disagreements: 0,
            planning_ms: start.elapsed().as_secs_f64() * 1e3,
        });
    }
    Ok(checked)
}

/// Checks every row of the archive at `path` against every planned query, table by table.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`ArchiveRows::for_each_table`]'s return values on failure.
fn check_rows(path: &std::path::Path, checked: &mut [Checked]) -> anyhow::Result<()> {
    let mut table_index = 0_usize;
    ArchiveRows::for_each_table(path, |timestamp_key, rows| {
        for check in checked.iter_mut() {
            let filter = match &check.planned {
                Planned::Pruned(_) => None,
                Planned::Plan(plan) => plan
                    .tables()
                    .iter()
                    .find(|table| table.table.get() as usize == table_index)
                    .map(|table| &table.filter),
            };
            for row in &rows {
                let is_reference_match =
                    ArchiveRows::reference_matches_with(timestamp_key, &check.query, row);
                let is_plan_match = match filter {
                    None => false,
                    Some(TableFilterPlan::MatchAll) => true,
                    Some(TableFilterPlan::Residual(residual)) => {
                        let mut evaluator = ResidualEvaluator {
                            query: &check.query,
                            residual,
                            prefilter_violations: Vec::new(),
                        };
                        let is_match = evaluator.matches(row);
                        check.num_disagreements += evaluator.prefilter_violations.len() as u64;
                        is_match
                    }
                };
                check.reference_count += u64::from(is_reference_match);
                check.plan_count += u64::from(is_plan_match);
                check.num_disagreements += u64::from(is_reference_match != is_plan_match);
            }
        }
        table_index += 1;
        Ok(())
    })?;
    Ok(())
}

/// # Returns
///
/// A one-line summary of a planning outcome.
fn summary(planned: &Planned) -> String {
    match planned {
        Planned::Pruned(report) => format!("pruned {report:?}"),
        Planned::Plan(plan) => {
            let num_match_all = plan
                .tables()
                .iter()
                .filter(|table| matches!(table.filter, TableFilterPlan::MatchAll))
                .count();
            format!(
                "{} candidate tables ({num_match_all} match-all), {} templates",
                plan.tables().len(),
                plan.templates().len()
            )
        }
    }
}

#[test]
#[ignore = "reads the large dataset archives"]
fn dataset_plans_agree_with_the_reference_and_the_cpp_counts() -> anyhow::Result<()> {
    let counts = oracle_counts()?;
    let queries = suite_queries()?;
    let mut failures = Vec::new();
    for dataset in ["bench", "spider", "synthetic"] {
        let path = data_root().join("datasets").join(format!("{dataset}.clps"));
        let archive = OpenedArchive::open(&path)?;
        let mut checked = plan_queries(dataset, &archive, &queries, &counts)?;
        check_rows(&path, &mut checked)?;
        for check in &checked {
            println!(
                "{dataset} {}: plan {} / reference {} / expected {:?}; {}; planned in {:.2} ms",
                check.id,
                check.plan_count,
                check.reference_count,
                check.expected_count,
                summary(&check.planned),
                check.planning_ms
            );
            if 0 != check.num_disagreements
                || check
                    .expected_count
                    .is_some_and(|count| count != check.plan_count)
            {
                failures.push(format!(
                    "{dataset} {}: {} disagreements, plan {}, reference {}, expected {:?}",
                    check.id,
                    check.num_disagreements,
                    check.plan_count,
                    check.reference_count,
                    check.expected_count
                ));
            }
        }
    }
    assert_eq!(failures, Vec::<String>::new());
    Ok(())
}
