//! Resolved projections against the C++ `clp-s s --projection` oracles.
//!
//! For every fixture query of the suite with a projection, the matching records are selected with
//! the plan and its compiled filters, and each one is rendered with only the stored leaves the
//! resolved projection names (a leaf is rendered iff a projected node has its path and a type
//! that can hold its value), as `{}` when it has none. The canonical multiset of those records
//! must equal what C++ printed.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clp_s_format::NodeId;
use clp_s_format::NodeType;
use clp_s_format::SchemaTree;
use clp_s_query::ColumnPattern;
use clp_s_query::Query;
use clp_s_query::QueryOptions;
use clp_s_search::CompiledTableFilter;
use clp_s_search::EvaluationScratch;
use clp_s_search::RecordProjection;
use clp_s_search::TableFilterPlan;
use clp_s_table::ResolvedProjection;

use crate::evaluator::selected_rows;
use crate::support::ArchiveRows;
use crate::support::OpenedArchive;
use crate::support::Planned;
use crate::support::plan;
use crate::support::prototype_dir;
use crate::support::readable_archives;
use crate::support::stem;
use crate::support::suite_queries;

/// # Returns
///
/// The keys from the namespace root to `node`, or `None` if a node is missing.
fn node_path(tree: &SchemaTree, node: NodeId) -> Option<Vec<String>> {
    let mut keys = Vec::new();
    let mut current = tree.node(node)?;
    while let Some(parent) = current.parent {
        keys.push(String::from_utf8_lossy(current.key).into_owned());
        current = tree.node(parent)?;
    }
    keys.reverse();
    Some(keys)
}

/// # Returns
///
/// Whether a node of `node_type` can hold the JSON value `value` as the C++ writer stores it.
fn holds(node_type: NodeType, value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => NodeType::NullValue == node_type,
        serde_json::Value::Bool(_) => NodeType::Boolean == node_type,
        serde_json::Value::Number(number) if number.is_f64() => matches!(
            node_type,
            NodeType::Float
                | NodeType::FormattedFloat
                | NodeType::DictionaryFloat
                | NodeType::Timestamp
        ),
        serde_json::Value::Number(_) => matches!(
            node_type,
            NodeType::Integer | NodeType::DeltaInteger | NodeType::Timestamp
        ),
        serde_json::Value::String(text) => {
            let encoded = if text.contains(' ') {
                NodeType::ClpString
            } else {
                NodeType::VarString
            };
            encoded == node_type || NodeType::Timestamp == node_type
        }
        serde_json::Value::Array(_) => NodeType::UnstructuredArray == node_type,
        serde_json::Value::Object(_) => NodeType::Object == node_type,
    }
}

/// # Returns
///
/// `record` restricted to the leaves of `projected` (paths with the node types that may hold
/// them).
fn project(record: &serde_json::Value, projected: &[(Vec<String>, NodeType)]) -> serde_json::Value {
    let mut output = serde_json::Value::Object(serde_json::Map::new());
    for (path, node_type) in projected {
        let Some(value) = path.iter().try_fold(record, |value, key| value.get(key)) else {
            continue;
        };
        if !holds(*node_type, value) {
            continue;
        }
        let mut target = &mut output;
        for (index, key) in path.iter().enumerate() {
            let Some(object) = target.as_object_mut() else {
                break;
            };
            let next = if index + 1 == path.len() {
                value.clone()
            } else {
                serde_json::Value::Object(serde_json::Map::new())
            };
            target = object.entry(key.clone()).or_insert(next);
        }
    }
    output
}

/// # Returns
///
/// The canonical text of `value`: keys sorted, numbers as `f64` when fractional.
fn canonical(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(object) => {
            let members: BTreeMap<&String, String> = object
                .iter()
                .map(|(key, value)| (key, canonical(value)))
                .collect();
            let members: Vec<String> = members
                .into_iter()
                .map(|(key, value)| format!("{key:?}:{value}"))
                .collect();
            format!("{{{}}}", members.join(","))
        }
        serde_json::Value::Array(elements) => {
            let elements: Vec<String> = elements.iter().map(canonical).collect();
            format!("[{}]", elements.join(","))
        }
        serde_json::Value::Number(number) if number.is_f64() => {
            format!("{:?}", number.as_f64().unwrap_or(f64::NAN))
        }
        other => other.to_string(),
    }
}

/// # Returns
///
/// The fixture archives a suite target names (`<target>.clps`, or its split parts).
fn target_archives(target: &str) -> Vec<PathBuf> {
    readable_archives()
        .into_iter()
        .filter(|path| {
            let name = stem(path);
            name == target
                || name
                    .strip_prefix(target)
                    .and_then(|rest| rest.strip_prefix('.'))
                    .is_some_and(|part| part.chars().all(|digit| digit.is_ascii_digit()))
        })
        .collect()
}

/// Selects the records of the archive at `path` matching `query` and renders them under the
/// resolved `projection`.
///
/// # Returns
///
/// The canonical rendered records on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`anyhow::Error`] if a planned table or projected node does not exist.
/// * Forwards [`ArchiveRows::load`]'s return values on failure.
/// * Forwards [`plan`]'s return values on failure.
/// * Forwards [`CompiledTableFilter::compile`]'s return values on failure.
/// * Forwards [`selected_rows`]'s return values on failure.
fn projected_records(
    path: &std::path::Path,
    query: &Arc<Query>,
    projection: &RecordProjection,
) -> anyhow::Result<Vec<String>> {
    let rows = ArchiveRows::load(path)?;
    let archive = OpenedArchive::open(path)?;
    let Planned::Plan(plan) = plan(&archive, query, projection)?.0 else {
        return Ok(Vec::new());
    };
    archive.load(plan.filter_dictionary_needs())?;
    let tree = archive.storage.index().schema_tree();
    let ResolvedProjection::Nodes(nodes) = plan.projection() else {
        anyhow::bail!("a column projection resolves to nodes");
    };
    let projected = nodes
        .iter()
        .map(|node| {
            let path = node_path(tree, *node).context("projected nodes exist")?;
            let node_type = tree.node(*node).context("projected nodes exist")?.node_type;
            Ok((path, node_type))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let tables = archive.load_tables()?;
    let mut scratch = EvaluationScratch::default();
    let mut records = Vec::new();
    for table_plan in plan.tables() {
        let ordinal = table_plan.table.get() as usize;
        let table_rows = rows.tables.get(ordinal).context("planned tables exist")?;
        let selected: Vec<u32> = match &table_plan.filter {
            TableFilterPlan::MatchAll => (0..table_plan.num_rows).collect(),
            TableFilterPlan::Residual(residual) => {
                let table = tables.get(ordinal).context("planned tables exist")?;
                let filter = CompiledTableFilter::compile(&plan, residual, table.layout())?;
                selected_rows(
                    table,
                    &filter,
                    clp_s_table::DEFAULT_BATCH_NUM_ROWS,
                    &mut scratch,
                )?
            }
        };
        for row in selected {
            let row = table_rows
                .get(row as usize)
                .context("selected rows exist")?;
            records.push(canonical(&project(&row.record, &projected)));
        }
    }
    Ok(records)
}

/// Records Rust matches and C++ does not, by case: `proj-poly` (`p: *`) also matches r24, whose
/// object `p` satisfies `Exists` under DV-19 and, objects never being projected, renders as `{}`.
const DIVERGENT_EXTRA_RECORDS: &[(&str, &[&str])] = &[("proj-poly", &["{}"])];

#[test]
fn projections_render_the_leaves_cpp_renders() -> anyhow::Result<()> {
    let mut num_cases = 0;
    for suite_query in suite_queries()? {
        if suite_query.projection.is_empty() || suite_query.target.starts_with("dataset:") {
            continue;
        }
        let options = QueryOptions::default().with_case_sensitivity(suite_query.is_case_sensitive);
        let query = Arc::new(Query::from_kql(&suite_query.kql, &options)?);
        let projection = RecordProjection::Columns(
            suite_query
                .projection
                .iter()
                .map(|column| ColumnPattern::parse(column))
                .collect::<Result<Vec<_>, _>>()?,
        );
        let mut actual = Vec::new();
        for path in target_archives(&suite_query.target) {
            actual.extend(projected_records(&path, &query, &projection)?);
        }
        let oracle = prototype_dir()
            .join("tests/queries/oracles")
            .join(format!("{}.jsonl", suite_query.id));
        let mut expected = std::fs::read_to_string(&oracle)
            .with_context(|| format!("reading {}", oracle.display()))?
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).map(|value| canonical(&value))
            })
            .collect::<Result<Vec<_>, _>>()?;
        for (id, extra) in DIVERGENT_EXTRA_RECORDS {
            if suite_query.id == *id {
                expected.extend(extra.iter().map(|record| (*record).to_owned()));
            }
        }
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected, "{}", suite_query.id);
        num_cases += 1;
    }
    assert!(num_cases >= 5, "only {num_cases} projection cases ran");
    Ok(())
}
