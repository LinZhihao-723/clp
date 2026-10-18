//! Projected templates against the C++ `clp-s s --projection` oracles of the query suite: every
//! record C++ printed for a filtered projection query is one of the records rendered for the
//! whole archive with the same projection.

use clp_s_format::NodeId;
use clp_s_format::NodeType;
use clp_s_format::SchemaTree;
use clp_s_table::DEFAULT_BATCH_NUM_ROWS;
use clp_s_table::ResolvedProjection;

use crate::support::EMULATED;
use crate::support::OpenedArchive;
use crate::support::fixture;
use crate::support::fixtures_dir;
use crate::support::missing_from;
use crate::support::read_lines;
use crate::support::sample;

/// Resolves literal projection paths as C++ `Projection::resolve_column` does: dot-separated keys
/// through objects of the default namespace; the last key matches every non-object node.
///
/// # Returns
///
/// The resolved projection.
fn resolve(tree: &SchemaTree, columns: &[&str]) -> ResolvedProjection {
    let mut nodes = Vec::new();
    let Some(root) = tree.subtree_root(b"", NodeType::Object) else {
        return ResolvedProjection::Nodes(Box::default());
    };
    for column in columns {
        let keys: Vec<&str> = column.split('.').collect();
        let mut parent = root;
        for (position, key) in keys.iter().enumerate() {
            let is_last = position + 1 == keys.len();
            let children = tree.children(parent).iter().copied().filter(|child| {
                tree.node(*child)
                    .is_some_and(|node| node.key == key.as_bytes())
            });
            let mut next_parent: Option<NodeId> = None;
            for child in children {
                let Some(node) = tree.node(child) else {
                    continue;
                };
                if is_last && NodeType::Object != node.node_type {
                    nodes.push(child);
                } else if !is_last && NodeType::Object == node.node_type {
                    next_parent = Some(child);
                    break;
                }
            }
            match next_parent {
                Some(next_parent) if !is_last => parent = next_parent,
                _ => break,
            }
        }
    }
    nodes.sort_unstable();
    nodes.dedup();
    ResolvedProjection::Nodes(nodes.into_boxed_slice())
}

#[test]
fn projected_records_include_every_cpp_projected_result() -> anyhow::Result<()> {
    let cases: [(&str, &[&str]); 5] = [
        ("proj-basic", &["rid", "level"]),
        ("proj-nested", &["req.user.name", "rid"]),
        (
            "proj-missing-columns",
            &["p", "objs.k", "req", "obj", "nosuch", "rid"],
        ),
        ("proj-poly", &["p"]),
        ("proj-all-missing", &["nosuch"]),
    ];
    let archive = OpenedArchive::open(&fixture("search.ts"))?;
    for (id, columns) in cases {
        let projection = resolve(archive.storage.index().schema_tree(), columns);
        let rendered = archive.render(&projection, EMULATED, DEFAULT_BATCH_NUM_ROWS)?;
        let expected = read_lines(
            &fixtures_dir()
                .join("../queries/oracles")
                .join(format!("{id}.jsonl")),
        )?;
        assert!(!expected.is_empty(), "{id}: the oracle has records");
        let missing = missing_from(&expected, &rendered);
        assert!(
            missing.is_empty(),
            "{id}: {} C++ records were not rendered, e.g. {:?}",
            missing.len(),
            sample(&missing)
        );
    }
    Ok(())
}

#[test]
fn records_without_projected_leaves_render_as_empty_objects() -> anyhow::Result<()> {
    let archive = OpenedArchive::open(&fixture("search.ts"))?;
    let projection = ResolvedProjection::Nodes(Box::default());
    let rendered = archive.render(&projection, EMULATED, DEFAULT_BATCH_NUM_ROWS)?;
    assert_eq!(rendered.len(), 40);
    assert!(
        rendered.iter().all(|record| b"{}" == record.as_slice()),
        "{:?}",
        rendered
            .first()
            .map(|record| String::from_utf8_lossy(record))
    );
    Ok(())
}

#[test]
fn projected_keys_follow_the_projected_insertion_order() -> anyhow::Result<()> {
    let archive = OpenedArchive::open(&fixture("search.ts"))?;
    let projection = resolve(
        archive.storage.index().schema_tree(),
        &["req.path", "rid", "req.user.name"],
    );
    let rendered = archive.render(&projection, EMULATED, DEFAULT_BATCH_NUM_ROWS)?;
    let record = rendered
        .iter()
        .find(|record| record.starts_with(b"{\"rid\":\"r01\""))
        .map(|record| String::from_utf8_lossy(record).into_owned());
    assert_eq!(
        record.as_deref(),
        Some(r#"{"rid":"r01","req":{"user":{"name":"bob"},"path":"/api/v1/disk"}}"#)
    );
    Ok(())
}
