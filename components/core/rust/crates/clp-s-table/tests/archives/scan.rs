//! Scanning behaviour over fixture tables: two-phase demand, windows, record ids and timestamps,
//! layouts and shapes, dictionaries and projections.

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::sync::Arc;

use anyhow::Context;
use clp_s_format::DictionaryKind;
use clp_s_format::DictionaryNeeds;
use clp_s_format::NodeType;
use clp_s_format::TableOrdinal;
use clp_s_io::ResolvedSource;
use clp_s_resources::ErrorKind;
use clp_s_table::ArrayEvent;
use clp_s_table::ArrayScalar;
use clp_s_table::ColumnBatch;
use clp_s_table::ColumnDemand;
use clp_s_table::ColumnEncoding;
use clp_s_table::ColumnIndex;
use clp_s_table::ColumnSet;
use clp_s_table::LayoutError;
use clp_s_table::ResolvedProjection;
use clp_s_table::ShapeEvent;
use clp_s_table::Table;
use clp_s_table::TableError;
use clp_s_table::TableScanner;
use clp_s_table::scan_array;

use crate::json::Json;
use crate::support::OpenedArchive;
use crate::support::Renderer;
use crate::support::fixture;
use crate::support::full_demand;
use crate::support::readable_archives;
use crate::support::scan_all;
use crate::support::stem;

/// # Returns
///
/// The table of `tables` with the most rows.
fn largest(tables: &[Arc<Table>]) -> Arc<Table> {
    Arc::clone(
        tables
            .iter()
            .max_by_key(|table| table.num_rows())
            .expect("the archive has tables"),
    )
}

/// # Returns
///
/// The first column of `table` with `encoding`.
fn column_with(table: &Table, encoding: ColumnEncoding) -> ColumnIndex {
    let position = table
        .layout()
        .columns()
        .iter()
        .position(|spec| encoding == spec.encoding)
        .unwrap_or_else(|| panic!("the table has a {encoding:?} column"));
    ColumnIndex::new(u32::try_from(position).expect("few columns"))
}

/// # Returns
///
/// The rendered records of `table` under its full demand, in row order, on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`scan_all`]'s return values on failure.
/// * Forwards [`Renderer::render`]'s return values on failure.
fn render_table(archive: &OpenedArchive, table: &Arc<Table>) -> anyhow::Result<Vec<Json>> {
    let timestamps = archive.storage.index().metadata().timestamp_index();
    let mut records = Vec::new();
    scan_all(table, full_demand(table), 4096, |batch| {
        for row in 0..batch.num_rows() {
            records.push(Renderer::render(&batch, row, timestamps)?);
        }
        Ok(())
    })?;
    Ok(records)
}

#[test]
fn output_columns_are_decoded_only_for_windows_with_a_selected_row() -> anyhow::Result<()> {
    let archive = OpenedArchive::open(&fixture("logs.ts"))?;
    let table = largest(&archive.load_tables()?);
    let expected = render_table(&archive, &table)?;
    let timestamps = archive.storage.index().metadata().timestamp_index();
    let layout = Arc::clone(table.layout());
    let num_columns = layout.num_columns();
    let evaluation_column = column_with(&table, ColumnEncoding::ClpString);
    let output_column = column_with(&table, ColumnEncoding::Timestamp);
    let mut evaluation = ColumnSet::empty(num_columns);
    evaluation.insert(evaluation_column);
    let demand = Arc::new(ColumnDemand::new(evaluation, ColumnSet::full(num_columns)));

    let window_size = 5;
    let mut scanner = TableScanner::new(NonZeroU32::new(window_size).context("non-zero")?);
    let mut state = scanner.start(Arc::clone(&table), demand);
    let mut num_windows = 0;
    let mut num_records_checked = 0;
    while let Some(window) = scanner.next_window(&mut state)? {
        assert_eq!(window.selected.count(), 0, "windows start unselected");
        assert!(matches!(
            window.columns.column(evaluation_column)?,
            ColumnBatch::ClpString(_)
        ));
        let result = window.columns.column(output_column);
        assert!(
            matches!(result, Err(TableError::ColumnNotDecoded(column)) if column == output_column),
            "{result:?}"
        );
        let first_row = window.columns.first_row();
        assert_eq!(first_row, num_windows * window_size);
        // Every third window selects nothing, so its output columns are skipped and the delta
        // anchors must still advance past it.
        let is_selecting = 0 != num_windows % 3;
        if is_selecting {
            window.selected.select(window.columns.num_rows() - 1);
        }
        let num_selected = scanner.finish_window(&state)?;
        let batch = scanner.batch(&state);
        if is_selecting {
            assert_eq!(num_selected, 1);
            let row = batch.num_rows() - 1;
            let rendered = Renderer::render(&batch, row, timestamps)?;
            assert_eq!(
                rendered,
                expected[(first_row + row) as usize],
                "row {}",
                first_row + row
            );
            num_records_checked += 1;
        } else {
            assert_eq!(num_selected, 0);
            let result = batch.column(output_column);
            assert!(
                matches!(result, Err(TableError::ColumnNotDecoded(_))),
                "{result:?}"
            );
            assert!(batch.column(evaluation_column).is_ok());
        }
        num_windows += 1;
    }
    assert!(state.is_finished());
    assert!(num_records_checked > 10, "{num_records_checked} records");
    Ok(())
}

#[test]
fn undemanded_columns_are_never_decoded() -> anyhow::Result<()> {
    let archive = OpenedArchive::open(&fixture("logs.ts"))?;
    let table = largest(&archive.load_tables()?);
    let num_columns = table.layout().num_columns();
    let demand = Arc::new(ColumnDemand::new(
        ColumnSet::empty(num_columns),
        ColumnSet::empty(num_columns),
    ));
    let log_event_idx_column = table
        .layout()
        .log_event_idx_column()
        .context("the archive records log order")?;
    let mut num_rows = 0;
    scan_all(&table, demand, 4096, |batch| {
        for column in 0..num_columns {
            let column = ColumnIndex::new(column);
            let result = batch.column(column);
            if column == log_event_idx_column {
                assert!(result.is_ok(), "record ids read the log_event_idx column");
            } else {
                assert!(
                    matches!(result, Err(TableError::ColumnNotDecoded(_))),
                    "{column:?}: {result:?}"
                );
            }
        }
        let result = batch.record_timestamp_ns(0);
        assert!(
            matches!(result, Err(TableError::ColumnNotDecoded(_))),
            "{result:?}"
        );
        num_rows += batch.num_rows();
        Ok(())
    })?;
    assert_eq!(num_rows, table.num_rows());
    Ok(())
}

#[test]
fn record_ids_are_log_event_indices_or_ordinals() -> anyhow::Result<()> {
    for path in readable_archives() {
        let archive = OpenedArchive::open(&path)?;
        let has_log_order = archive.storage.index().has_log_order();
        let mut record_ids = Vec::new();
        for table in archive.load_tables()? {
            let num_columns = table.layout().num_columns();
            let demand = Arc::new(ColumnDemand::new(
                ColumnSet::empty(num_columns),
                ColumnSet::empty(num_columns),
            ));
            scan_all(&table, demand, 3, |batch| {
                for row in 0..batch.num_rows() {
                    let record_id = batch.record_id(row)?.get();
                    assert_eq!(
                        batch.log_event_idx(row)?,
                        has_log_order.then_some(record_id),
                        "{}",
                        stem(&path)
                    );
                    record_ids.push(record_id);
                }
                Ok(())
            })?;
        }
        if !has_log_order {
            assert!(
                record_ids.is_sorted(),
                "{}: ordinals follow directory order",
                stem(&path)
            );
        }
        record_ids.sort_unstable();
        let num_records = i64::try_from(record_ids.len())?;
        assert_eq!(
            record_ids,
            (0..num_records).collect::<Vec<_>>(),
            "{}: record ids are dense",
            stem(&path)
        );
    }
    Ok(())
}

#[test]
fn record_timestamps_come_from_the_authoritative_timestamp_column() -> anyhow::Result<()> {
    let mut num_archives_with_timestamps = 0;
    for path in readable_archives() {
        let archive = OpenedArchive::open(&path)?;
        let authoritative = archive
            .storage
            .index()
            .metadata()
            .timestamp_index()
            .authoritative()
            .cloned();
        let mut num_timestamps = 0;
        for table in archive.load_tables()? {
            let layout = Arc::clone(table.layout());
            let Some(column) = layout.record_timestamp_column() else {
                if let Some(entry) = &authoritative {
                    assert_eq!(
                        layout.columns_for_node(entry.node_id),
                        &[],
                        "{}",
                        stem(&path)
                    );
                }
                scan_all(&table, full_demand(&table), 4096, |batch| {
                    assert_eq!(batch.record_timestamp_ns(0)?, None);
                    Ok(())
                })?;
                continue;
            };
            let entry = authoritative.as_ref().context("a timestamp column")?;
            assert_eq!(
                layout.columns()[column.get() as usize].node_id,
                entry.node_id
            );
            scan_all(&table, full_demand(&table), 4, |batch| {
                let ColumnBatch::Timestamp { epoch_ns, .. } = batch.column(column)? else {
                    anyhow::bail!("the record timestamp column is a timestamp column");
                };
                for row in 0..batch.num_rows() {
                    let timestamp = batch
                        .record_timestamp_ns(row)?
                        .context("the table has a record timestamp")?;
                    assert_eq!(timestamp, epoch_ns[row as usize]);
                    if let Some(clp_s_format::TimestampRange::EpochMillis { begin, end }) =
                        entry.range
                    {
                        let timestamp_ms = timestamp.div_euclid(1_000_000);
                        assert!(
                            (begin..=end).contains(&timestamp_ms),
                            "{}: {timestamp_ms} outside [{begin}, {end}]",
                            stem(&path)
                        );
                    }
                    num_timestamps += 1;
                }
                Ok(())
            })?;
        }
        if 0 != num_timestamps {
            num_archives_with_timestamps += 1;
        }
    }
    assert!(
        num_archives_with_timestamps > 10,
        "{num_archives_with_timestamps} archives"
    );
    Ok(())
}

#[test]
fn shapes_hold_every_default_namespace_column_once() -> anyhow::Result<()> {
    for path in readable_archives() {
        let archive = OpenedArchive::open(&path)?;
        let index = archive.storage.index();
        let tree = index.schema_tree();
        let default_root = tree.subtree_root(b"", NodeType::Object);
        for info in index.tables().tables() {
            let layout = archive.tables.layout(info.ordinal)?;
            let events = layout.shape().events();
            assert_eq!(
                events.first(),
                Some(&ShapeEvent::BeginObject { node_id: None })
            );
            assert_eq!(events.last(), Some(&ShapeEvent::EndObject));
            let mut depth = 0_i32;
            let mut shape_columns = HashSet::new();
            for event in events {
                match event {
                    ShapeEvent::BeginObject { .. } => depth += 1,
                    ShapeEvent::EndObject => depth -= 1,
                    ShapeEvent::Column { node_id, column } => {
                        assert_eq!(layout.columns_for_node(*node_id), &[*column]);
                        assert!(shape_columns.insert(*column), "{column:?} repeats");
                    }
                    ShapeEvent::Null { node_id } | ShapeEvent::EmptyObject { node_id } => {
                        assert_eq!(layout.columns_for_node(*node_id), &[]);
                    }
                }
                assert!(depth >= 0, "{}: unbalanced shape", stem(&path));
            }
            assert_eq!(depth, 0, "{}: unbalanced shape", stem(&path));

            for (position, spec) in layout.columns().iter().enumerate() {
                let column = ColumnIndex::new(u32::try_from(position)?);
                let mut root = spec.node_id;
                while let Some(parent) = tree.node(root).and_then(|node| node.parent) {
                    root = parent;
                }
                assert_eq!(
                    shape_columns.contains(&column),
                    Some(root) == default_root,
                    "{}: column {position} is in the shape iff it is in the default namespace",
                    stem(&path)
                );
            }
        }
    }
    Ok(())
}

#[test]
fn projected_shapes_keep_only_the_projected_leaves() -> anyhow::Result<()> {
    let mut num_partial_projections = 0;
    for path in readable_archives() {
        let archive = OpenedArchive::open(&path)?;
        let index = archive.storage.index();
        for info in index.tables().tables() {
            let layout = archive.tables.layout(info.ordinal)?;
            let schema_nodes = index
                .schema_map()
                .schema(info.schema_id)
                .context("the schema exists")?
                .nodes;
            assert_eq!(
                &layout.projected_shape(index, &ResolvedProjection::Full)?,
                layout.shape(),
                "{}",
                stem(&path)
            );
            let every_node = ResolvedProjection::Nodes(schema_nodes.into());
            assert_eq!(
                &layout.projected_shape(index, &every_node)?,
                layout.shape(),
                "{}",
                stem(&path)
            );

            let Some(&first_column) = layout.columns().first() else {
                continue;
            };
            let projected = layout.projected_shape(
                index,
                &ResolvedProjection::Nodes(vec![first_column.node_id].into()),
            )?;
            let projected_leaves: Vec<&ShapeEvent> = projected
                .events()
                .iter()
                .filter(|event| {
                    !matches!(
                        event,
                        ShapeEvent::BeginObject { .. } | ShapeEvent::EndObject
                    )
                })
                .collect();
            let is_in_shape = layout.shape().events().iter().any(|event| {
                matches!(
                    event,
                    ShapeEvent::Column { node_id, .. } if *node_id == first_column.node_id
                )
            });
            if is_in_shape {
                assert_eq!(
                    projected_leaves,
                    vec![&ShapeEvent::Column {
                        node_id: first_column.node_id,
                        column: ColumnIndex::new(0),
                    }],
                    "{}",
                    stem(&path)
                );
                num_partial_projections += 1;
            } else {
                assert_eq!(
                    projected_leaves,
                    Vec::<&ShapeEvent>::new(),
                    "{}",
                    stem(&path)
                );
            }
        }
    }
    assert!(
        num_partial_projections > 10,
        "{num_partial_projections} projections"
    );
    Ok(())
}

#[test]
fn layouts_are_compiled_once_per_table() -> anyhow::Result<()> {
    let archive = OpenedArchive::open(&fixture("many-schemas.default"))?;
    let num_tables = archive.storage.index().tables().tables().len();
    for ordinal in 0..u32::try_from(num_tables)? {
        let first = archive.tables.layout(TableOrdinal::new(ordinal))?;
        let second = archive.tables.layout(TableOrdinal::new(ordinal))?;
        assert!(Arc::ptr_eq(&first, &second), "table {ordinal}");
    }
    let unknown = TableOrdinal::new(u32::try_from(num_tables)?);
    let result = archive.tables.layout(unknown);
    assert!(
        matches!(&result, Err(LayoutError::UnknownTable(table)) if *table == unknown),
        "{result:?}"
    );
    assert_eq!(
        result.map(|_| ()).map_err(|error| error.kind()),
        Err(ErrorKind::Internal)
    );
    Ok(())
}

#[test]
fn dictionary_needs_follow_the_demanded_columns() -> anyhow::Result<()> {
    let archive = OpenedArchive::open(&fixture("arrays.default"))?;
    let mut num_checked = 0;
    for table in archive.load_tables()? {
        let layout = table.layout();
        let num_columns = layout.num_columns();
        for (position, spec) in layout.columns().iter().enumerate() {
            let mut set = ColumnSet::empty(num_columns);
            set.insert(ColumnIndex::new(u32::try_from(position)?));
            let expected = spec.node_type.referenced_dictionaries();
            let evaluation_only = ColumnDemand::new(set.clone(), ColumnSet::empty(num_columns));
            let output_only = ColumnDemand::new(ColumnSet::empty(num_columns), set);
            assert_eq!(evaluation_only.dictionary_needs(layout), expected);
            assert_eq!(output_only.dictionary_needs(layout), expected);
            if ColumnEncoding::UnstructuredArray == spec.encoding {
                assert_eq!(
                    expected,
                    DictionaryNeeds {
                        is_variable_needed: true,
                        is_logtype_needed: false,
                        is_array_needed: true,
                    }
                );
                num_checked += 1;
            }
        }
        let none = ColumnDemand::new(ColumnSet::empty(num_columns), ColumnSet::empty(num_columns));
        assert_eq!(none.dictionary_needs(layout), DictionaryNeeds::default());
    }
    assert!(num_checked > 0);
    Ok(())
}

#[test]
fn columns_of_unloaded_dictionaries_are_reported() -> anyhow::Result<()> {
    let path = fixture("strings.default");
    let archive = OpenedArchive::open_source(
        ResolvedSource::from_path(&path)?,
        DictionaryNeeds::default(),
    )?;
    let mut kinds = HashSet::new();
    for table in archive.load_tables()? {
        let layout = Arc::clone(table.layout());
        scan_all(&table, full_demand(&table), 4096, |batch| {
            for (position, spec) in layout.columns().iter().enumerate() {
                let result = batch.column(ColumnIndex::new(u32::try_from(position)?));
                match spec.encoding {
                    ColumnEncoding::ClpString
                    | ColumnEncoding::VariableString
                    | ColumnEncoding::DictionaryFloat
                    | ColumnEncoding::UnstructuredArray => {
                        let Err(TableError::DictionaryNotLoaded(kind)) = result else {
                            anyhow::bail!("{:?} column: {result:?}", spec.encoding);
                        };
                        kinds.insert(kind);
                    }
                    _ => {
                        result?;
                    }
                }
            }
            Ok(())
        })?;
    }
    assert!(kinds.contains(&DictionaryKind::LogType), "{kinds:?}");
    assert!(kinds.contains(&DictionaryKind::Variable), "{kinds:?}");
    Ok(())
}

#[test]
fn projections_select_the_columns_of_their_nodes() -> anyhow::Result<()> {
    let archive = OpenedArchive::open(&fixture("nested.default"))?;
    for table in archive.load_tables()? {
        let layout = table.layout();
        assert_eq!(
            ResolvedProjection::Full.output_columns(layout),
            ColumnSet::full(layout.num_columns())
        );
        let Some(spec) = layout.columns().last() else {
            continue;
        };
        let projection = ResolvedProjection::Nodes(vec![spec.node_id].into());
        let columns = projection.output_columns(layout);
        assert_eq!(columns.to_vec(), layout.columns_for_node(spec.node_id));
        let empty = ResolvedProjection::Nodes(Vec::new().into()).output_columns(layout);
        assert_eq!(empty.to_vec(), Vec::<ColumnIndex>::new());
    }
    Ok(())
}

#[test]
#[should_panic(expected = "does not belong to this scan")]
fn finishing_another_scans_window_panics() {
    let archive = OpenedArchive::open(&fixture("scalars.default")).expect("the fixture opens");
    let table = largest(&archive.load_tables().expect("the tables load"));
    let mut scanner = TableScanner::new(NonZeroU32::MIN);
    let mut first = scanner.start(Arc::clone(&table), full_demand(&table));
    let second = scanner.start(Arc::clone(&table), full_demand(&table));
    scanner
        .next_window(&mut first)
        .expect("the window decodes")
        .expect("the table has rows");
    let _ = scanner.finish_window(&second);
}

/// # Returns
///
/// `value` with every number token rewritten as the scanner types it, so it can be compared with
/// the tree built from [`ArrayEvent`]s.
fn typed_numbers(value: Json) -> Json {
    match value {
        Json::Number(token) => Json::Number(
            token
                .parse::<i64>()
                .map(|value| value.to_string())
                .or_else(|_| token.parse::<u64>().map(|value| value.to_string()))
                .unwrap_or(token),
        ),
        Json::Array(values) => Json::Array(values.into_iter().map(typed_numbers).collect()),
        Json::Object(members) => Json::Object(
            members
                .into_iter()
                .map(|(key, value)| (key, typed_numbers(value)))
                .collect(),
        ),
        other => other,
    }
}

/// Builds a [`Json`] tree from the events of one scanned array.
#[derive(Default)]
struct EventTree {
    stack: Vec<(Option<Vec<u8>>, Json)>,
    pending_key: Option<Vec<u8>>,
    root: Option<Json>,
}

impl EventTree {
    /// Adds `value` to the innermost open container.
    fn push(&mut self, value: Json) {
        let key = self.pending_key.take();
        match self.stack.last_mut() {
            Some((_, Json::Array(values))) => values.push(value),
            Some((_, Json::Object(members))) => {
                members.push((key.expect("object members have keys"), value));
            }
            _ => self.root = Some(value),
        }
    }

    /// Applies one event.
    fn apply(&mut self, event: ArrayEvent<'_>) {
        match event {
            ArrayEvent::BeginArray => self
                .stack
                .push((self.pending_key.take(), Json::Array(Vec::new()))),
            ArrayEvent::BeginObject => {
                self.stack
                    .push((self.pending_key.take(), Json::Object(Vec::new())));
            }
            ArrayEvent::EndArray | ArrayEvent::EndObject => {
                let (key, value) = self.stack.pop().expect("a container is open");
                self.pending_key = key;
                self.push(value);
            }
            ArrayEvent::Key(key) => self.pending_key = Some(key.into_owned()),
            ArrayEvent::Scalar(scalar) => self.push(match scalar {
                ArrayScalar::Null => Json::Null,
                ArrayScalar::Boolean(value) => Json::Bool(value),
                ArrayScalar::Integer(value) => Json::Number(value.to_string()),
                ArrayScalar::UnsignedInteger(value) => Json::Number(value.to_string()),
                ArrayScalar::Float(value) => Json::Number(format!("{value:?}")),
                ArrayScalar::String(value) => Json::String(value.into_owned()),
            }),
        }
    }
}

#[test]
fn every_fixture_array_scans_to_its_json_tree() -> anyhow::Result<()> {
    let mut num_arrays = 0;
    let mut text = Vec::new();
    for path in readable_archives() {
        let archive = OpenedArchive::open(&path)?;
        for table in archive.load_tables()? {
            let layout = Arc::clone(table.layout());
            scan_all(&table, full_demand(&table), 4096, |batch| {
                for (position, spec) in layout.columns().iter().enumerate() {
                    if ColumnEncoding::UnstructuredArray != spec.encoding {
                        continue;
                    }
                    let ColumnBatch::UnstructuredArray(arrays) =
                        batch.column(ColumnIndex::new(u32::try_from(position)?))?
                    else {
                        anyhow::bail!("an unstructured-array column");
                    };
                    for row in 0..batch.num_rows() {
                        text.clear();
                        arrays.decode(row, &mut text)?;
                        let mut tree = EventTree::default();
                        scan_array(&text, |event| {
                            tree.apply(event);
                            true
                        })?;
                        assert_eq!(
                            tree.root,
                            Some(typed_numbers(Json::parse(&text)?)),
                            "{}: {}",
                            stem(&path),
                            String::from_utf8_lossy(&text)
                        );
                        num_arrays += 1;
                    }
                }
                Ok(())
            })?;
        }
    }
    assert!(num_arrays > 50, "{num_arrays} arrays");
    Ok(())
}
