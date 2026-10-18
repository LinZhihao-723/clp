//! The planner's protocol and plan shapes on fixture archives: the dictionary protocol, prune
//! reports per stage, match-all tables, template sharing, bindings, projections, limits and the
//! EXPLAIN text.

use std::sync::Arc;

use clp_s_format::DictionaryKind;
use clp_s_format::DictionaryNeeds;
use clp_s_format::NodeId;
use clp_s_format::NodeType;
use clp_s_query::ColumnPattern;
use clp_s_query::Query;
use clp_s_query::QueryOptions;
use clp_s_query::TimeBounds;
use clp_s_search::ArrayAccess;
use clp_s_search::ColumnTest;
use clp_s_search::PlanError;
use clp_s_search::PlanLimits;
use clp_s_search::PlanStep;
use clp_s_search::PruneReason;
use clp_s_search::PruneReport;
use clp_s_search::RecordProjection;
use clp_s_search::SearchPlanner;
use clp_s_search::TableFilterPlan;
use clp_s_search::TerminationStage;
use clp_s_table::ResolvedProjection;

use crate::support::OpenedArchive;
use crate::support::Planned;
use crate::support::fixture;
use crate::support::plan;

/// # Returns
///
/// The normalized query of `kql`.
fn query(kql: &str) -> Arc<Query> {
    Arc::new(Query::from_kql(kql, &QueryOptions::default()).expect("the test query should parse"))
}

/// # Returns
///
/// The plan of `kql` on fixture `name`, panicking if the archive is pruned.
fn planned(name: &str, kql: &str) -> Arc<clp_s_search::ArchiveSearchPlan> {
    let archive = OpenedArchive::open(&fixture(name)).expect("the fixture should open");
    match plan(&archive, &query(kql), &RecordProjection::Full)
        .expect("planning should succeed")
        .0
    {
        Planned::Plan(plan) => plan,
        Planned::Pruned(report) => panic!("{kql} on {name} is pruned: {report:?}"),
    }
}

/// # Returns
///
/// The node `path` (literal keys from the default root, then `node_type`) of the fixture.
fn node(archive: &OpenedArchive, path: &[&str], node_type: NodeType) -> NodeId {
    let tree = archive.storage.index().schema_tree();
    let mut current = tree
        .subtree_root(b"", NodeType::Object)
        .expect("the archive has a default namespace");
    for (index, key) in path.iter().enumerate() {
        let is_last = index + 1 == path.len();
        current = *tree
            .children(current)
            .iter()
            .find(|child| {
                tree.node(**child).is_some_and(|view| {
                    view.key == key.as_bytes()
                        && (if is_last {
                            node_type == view.node_type
                        } else {
                            NodeType::Object == view.node_type
                        })
                })
            })
            .unwrap_or_else(|| panic!("the archive has the node {path:?}"));
    }
    current
}

/// # Returns
///
/// Whether `test` reads array values through `expected`.
fn is_array_access(test: &ColumnTest, expected: &ArrayAccess) -> bool {
    matches!(test, ColumnTest::Array(access) if access == expected)
}

#[test]
fn dictionaries_are_requested_once_and_required() -> anyhow::Result<()> {
    let archive = OpenedArchive::open(&fixture("search.ts"))?;
    let planner = SearchPlanner::new(
        query(r#"msg: "*failed*""#),
        &RecordProjection::Full,
        PlanLimits::default(),
    )?;
    assert_eq!(
        planner.prune_on_metadata(archive.storage.index().metadata()),
        None
    );
    let mut archive_planner = planner.plan_archive(Arc::clone(&archive.tables));
    let PlanStep::NeedsDictionaries(needs) = archive_planner.advance()? else {
        panic!("a ClpString prefilter needs dictionaries");
    };
    assert_eq!(
        needs,
        DictionaryNeeds {
            is_variable_needed: true,
            is_logtype_needed: true,
            is_array_needed: false,
        }
    );
    assert!(matches!(
        archive_planner.advance(),
        Err(PlanError::DictionaryNotLoaded(DictionaryKind::Variable))
    ));

    archive.load(needs)?;
    let PlanStep::Planned(plan) = archive_planner.advance()? else {
        panic!("the plan follows the dictionaries");
    };
    let PlanStep::Planned(again) = archive_planner.advance()? else {
        panic!("a final step repeats");
    };
    assert!(
        Arc::ptr_eq(&plan, &again),
        "advancing again returns the same plan"
    );
    assert!(plan.filter_dictionary_needs().is_logtype_needed);

    let is_encoded_text_bound = plan.tables().iter().any(|table| match &table.filter {
        TableFilterPlan::Residual(residual) => residual.bindings().iter().any(|binding| {
            binding
                .columns
                .iter()
                .any(|column| matches!(column.test, ColumnTest::EncodedText(_)))
        }),
        TableFilterPlan::MatchAll => false,
    });
    assert!(is_encoded_text_bound, "{plan}");
    Ok(())
}

#[test]
fn every_stage_reports_why_it_pruned() -> anyhow::Result<()> {
    for (name, kql, bounds, expected) in [
        (
            "search.ts",
            "NOT *",
            TimeBounds::default(),
            (
                TerminationStage::LogicallyFalse,
                PruneReason::LogicallyFalse,
            ),
        ),
        (
            "search.ts",
            "*",
            TimeBounds::from_epoch_millis(Some(1_800_000_000_000), None),
            (
                TerminationStage::TimeRangeMatching,
                PruneReason::TimeRangeDisjoint,
            ),
        ),
        (
            "search.default",
            "*",
            TimeBounds::from_epoch_millis(Some(0), None),
            (
                TerminationStage::TimeRangeMatching,
                PruneReason::NoTimestampColumn,
            ),
        ),
        (
            "search.ts",
            r#"$_filename: "other.jsonl""#,
            TimeBounds::default(),
            (
                TerminationStage::RangeIndexMatching,
                PruneReason::RangeIndexMismatch,
            ),
        ),
        (
            "search.ts",
            "zzz: 1",
            TimeBounds::default(),
            (
                TerminationStage::SchemaMatching,
                PruneReason::NoCandidateTable,
            ),
        ),
        (
            "search.ts",
            "level: NOSUCHLEVEL",
            TimeBounds::default(),
            (
                TerminationStage::DictionarySearch,
                PruneReason::DictionaryMiss,
            ),
        ),
        (
            "search.ts",
            "ts > 1800000000000",
            TimeBounds::default(),
            (
                TerminationStage::TimeRangeMatchingAfterColumnResolution,
                PruneReason::TimeRangeDisjoint,
            ),
        ),
    ] {
        let archive = OpenedArchive::open(&fixture(name))?;
        let query =
            Arc::new(Query::from_kql(kql, &QueryOptions::default())?.with_time_bounds(bounds));
        let (outcome, _) = plan(&archive, &query, &RecordProjection::Full)?;
        let Planned::Pruned(report) = outcome else {
            panic!("{kql} on {name} should be pruned");
        };
        assert_eq!(
            report,
            PruneReport {
                stage: expected.0,
                reason: expected.1,
            },
            "{kql} on {name}"
        );
    }
    Ok(())
}

#[test]
fn proven_tables_are_match_all_without_dictionaries() -> anyhow::Result<()> {
    let archive = OpenedArchive::open(&fixture("search.ts"))?;
    for kql in [
        "level: *",
        "ts >= 1600000000000",
        "ts <= 1700000039000 AND level: *",
    ] {
        let (outcome, num_requests) = plan(&archive, &query(kql), &RecordProjection::Full)?;
        let Planned::Plan(plan) = outcome else {
            panic!("{kql} should plan");
        };
        assert_eq!(num_requests, 0, "{kql} reads no dictionary");
        assert_eq!(plan.templates(), &[], "{kql}:\n{plan}");
        assert!(
            plan.tables()
                .iter()
                .all(|table| matches!(table.filter, TableFilterPlan::MatchAll)),
            "{kql}:\n{plan}"
        );
    }
    Ok(())
}

#[test]
fn the_timestamp_index_binds_its_column_by_node_id() -> anyhow::Result<()> {
    let archive = OpenedArchive::open(&fixture("timestamps-escaped-key.ts-escaped"))?;
    let (outcome, _) = plan(
        &archive,
        &query(r"t\*s > 1800000000000"),
        &RecordProjection::Full,
    )?;
    let Planned::Pruned(report) = outcome else {
        panic!("the index of the escaped key `t*s` should prune the archive");
    };
    assert_eq!(
        report,
        PruneReport {
            stage: TerminationStage::TimeRangeMatchingAfterColumnResolution,
            reason: PruneReason::TimeRangeDisjoint,
        }
    );

    let (outcome, _) = plan(
        &archive,
        &query(r"t\*s < 1800000000000"),
        &RecordProjection::Full,
    )?;
    let Planned::Plan(covering) = outcome else {
        panic!("a range covering the index should plan");
    };
    assert!(
        covering
            .tables()
            .iter()
            .all(|table| matches!(table.filter, TableFilterPlan::MatchAll)),
        "the index proves every row:\n{covering}"
    );

    let unindexed = planned("timestamps-escaped-key.ts-escaped", "ts > 1800000000000");
    assert!(
        unindexed
            .tables()
            .iter()
            .all(|table| matches!(table.filter, TableFilterPlan::Residual(_))),
        "the integer column `ts` is not the indexed node:\n{unindexed}"
    );
    Ok(())
}

#[test]
fn the_timestamp_index_is_not_bound_to_the_same_key_in_another_namespace() -> anyhow::Result<()> {
    let archive = OpenedArchive::open(&fixture("kvir-text.ts"))?;
    for (kql, expected_report) in [
        (
            "timestamp < 1000",
            PruneReport {
                stage: TerminationStage::TimeRangeMatchingAfterColumnResolution,
                reason: PruneReason::TimeRangeDisjoint,
            },
        ),
        (
            "@timestamp < 1000",
            PruneReport {
                stage: TerminationStage::SchemaMatching,
                reason: PruneReason::NoCandidateTable,
            },
        ),
        (
            "NOT @timestamp > 1000",
            PruneReport {
                stage: TerminationStage::SchemaMatching,
                reason: PruneReason::NoCandidateTable,
            },
        ),
    ] {
        let (outcome, _) = plan(&archive, &query(kql), &RecordProjection::Full)?;
        let Planned::Pruned(report) = outcome else {
            panic!("{kql} should be pruned");
        };
        assert_eq!(report, expected_report, "{kql}");
    }
    Ok(())
}

#[test]
fn tables_with_the_same_leaf_states_share_a_template() {
    let plan = planned(
        "many-schemas.default",
        "id > 10 AND NOT msg: \"*worker 1*\"",
    );
    let num_residual_tables = plan
        .tables()
        .iter()
        .filter(|table| matches!(table.filter, TableFilterPlan::Residual(_)))
        .count();
    assert!(num_residual_tables > 4, "{plan}");
    assert!(
        plan.templates().len() < num_residual_tables / 2,
        "templates are shared:\n{plan}"
    );
}

#[test]
fn bindings_name_the_columns_and_their_tests() -> anyhow::Result<()> {
    let archive = OpenedArchive::open(&fixture("search.ts"))?;
    let n_integer = node(&archive, &["n"], NodeType::Integer);
    let n_text = node(&archive, &["n"], NodeType::VarString);
    let (outcome, _) = plan(&archive, &query("n: 5"), &RecordProjection::Full)?;
    let Planned::Plan(n_plan) = outcome else {
        panic!("n: 5 should plan");
    };
    let mut tests = Vec::new();
    for table in n_plan.tables() {
        let TableFilterPlan::Residual(residual) = &table.filter else {
            continue;
        };
        for binding in residual.bindings() {
            for column in &binding.columns {
                tests.push((
                    column.node_id,
                    matches!(column.test, ColumnTest::Value),
                    matches!(column.test, ColumnTest::VariableIds(_)),
                ));
            }
        }
    }
    assert!(tests.contains(&(n_integer, true, false)), "{n_plan}");
    assert!(tests.contains(&(n_text, false, true)), "{n_plan}");

    let arrays = OpenedArchive::open(&fixture("arrays.default"))?;
    for (kql, expected) in [
        ("a: 1", ArrayAccess::Walk(Box::new([]))),
        ("*: 1", ArrayAccess::AnyLeaf),
    ] {
        let (outcome, _) = plan(&arrays, &query(kql), &RecordProjection::Full)?;
        let Planned::Plan(array_plan) = outcome else {
            panic!("{kql} should plan");
        };
        let is_bound = array_plan.tables().iter().any(|table| match &table.filter {
            TableFilterPlan::Residual(residual) => residual.bindings().iter().any(|binding| {
                binding
                    .columns
                    .iter()
                    .any(|column| is_array_access(&column.test, &expected))
            }),
            TableFilterPlan::MatchAll => false,
        });
        assert!(is_bound, "{kql}:\n{array_plan}");
    }
    Ok(())
}

#[test]
fn projections_follow_the_cpp_rules() -> anyhow::Result<()> {
    let archive = OpenedArchive::open(&fixture("search.ts"))?;
    let columns = [
        "req.user.name",
        "level",
        "req",
        "req.user.id.x",
        "tags",
        "zzz",
    ]
    .iter()
    .map(|column| ColumnPattern::parse(column))
    .collect::<Result<Vec<_>, _>>()?;
    let (outcome, _) = plan(&archive, &query("*"), &RecordProjection::Columns(columns))?;
    let Planned::Plan(plan) = outcome else {
        panic!("a projection-only query plans");
    };
    let mut expected = vec![
        node(&archive, &["req", "user", "name"], NodeType::VarString),
        node(&archive, &["level"], NodeType::VarString),
        node(&archive, &["level"], NodeType::ClpString),
        node(&archive, &["tags"], NodeType::UnstructuredArray),
    ];
    expected.sort_unstable();
    assert_eq!(
        *plan.projection(),
        ResolvedProjection::Nodes(expected.into_boxed_slice())
    );

    let columns = vec![ColumnPattern::parse("a.*")?];
    let result = SearchPlanner::new(
        query("*"),
        &RecordProjection::Columns(columns.clone()),
        PlanLimits::default(),
    );
    assert!(
        matches!(result, Err(PlanError::InvalidProjection { .. })),
        "{columns:?} should be rejected"
    );

    let resolved = |columns: &[&str]| -> anyhow::Result<ResolvedProjection> {
        let columns = columns
            .iter()
            .map(|column| ColumnPattern::parse(column))
            .collect::<Result<Vec<_>, _>>()?;
        let (outcome, _) =
            crate::support::plan(&archive, &query("*"), &RecordProjection::Columns(columns))?;
        let Planned::Plan(planned) = outcome else {
            panic!("a projection-only query plans");
        };
        Ok(planned.projection().clone())
    };
    assert_eq!(
        resolved(&["level", "rid", "level"])?,
        resolved(&["level", "rid"])?,
        "a repeated column projects its nodes once"
    );
    Ok(())
}

#[test]
fn plans_beyond_the_limits_fail() -> anyhow::Result<()> {
    let archive = OpenedArchive::open(&fixture("search.ts"))?;
    let planner = SearchPlanner::new(
        query("level: (INFO WARN ERROR)"),
        &RecordProjection::Full,
        PlanLimits { max_plan_size: 0 },
    )?;
    archive.load(crate::support::ALL_DICTIONARIES)?;
    let result = planner.plan_archive(Arc::clone(&archive.tables)).advance();
    assert!(
        matches!(result, Err(PlanError::LimitExceeded { limit_size: 0, .. })),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn explain_lists_leaves_templates_tables_and_needs() {
    let plan = planned("search.ts", r#"msg: "*failed*" AND n > 1"#);
    let text = plan.to_string();
    println!("{text}");
    for expected in [
        "leaves:",
        "L0: ",
        "templates:",
        "T0: ",
        "tables:",
        "text [\"*failed*\"",
        "value",
        "projection: full",
        "filter dictionaries: variable, logtype",
    ] {
        assert!(
            text.contains(expected),
            "{expected:?} is missing from:\n{text}"
        );
    }
}
