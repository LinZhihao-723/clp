//! Compiled JSON templates.

use std::sync::Arc;

use clp_s_format::NodeId;
use clp_s_format::SchemaId;
use clp_s_format::SchemaTree;
use clp_s_table::ColumnEncoding;
use clp_s_table::ColumnIndex;
use clp_s_table::LayoutError;
use clp_s_table::ShapeEvent;
use clp_s_table::TableBatch;
use clp_s_table::TableError;
use clp_s_table::TableLayout;

use crate::JsonError;
use crate::JsonOptions;
use crate::JsonRecordBuffer;
use crate::RenderContext;
use crate::escape::escape_into;
use crate::writer::ValueWriter;

/// A compiled program for one (layout, projection, options): static fragments and value slots.
#[derive(Debug)]
pub struct JsonTemplate {
    schema_id: SchemaId,
    options: JsonOptions,

    /// Every static fragment back to back: braces, commas, escaped keys, string quotes, nulls and
    /// empty objects.
    fragments: Box<[u8]>,

    /// The value slots in output order; each is preceded by the fragment bytes from the previous
    /// slot's end to its own `fragment_end`, and the last is followed by the rest of `fragments`.
    slots: Box<[Slot]>,
}

impl JsonTemplate {
    /// Binds the template to one batch and the archive's render context, resolving every slot's
    /// column so the row loop does no lookups.
    ///
    /// # Returns
    ///
    /// The bound template on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`JsonError::UnexpectedColumn`] if a slot's column does not hold the values the template
    ///   was compiled for.
    /// * Forwards [`TableBatch::column`]'s return values on failure.
    ///
    /// # Panics
    ///
    /// Panics if `batch` belongs to another archive than `context`, or to a table of another
    /// schema than the one the template was compiled for.
    pub fn bind<'batch>(
        &'batch self,
        batch: TableBatch<'batch>,
        context: &'batch RenderContext,
    ) -> Result<BoundJsonTemplate<'batch>, JsonError> {
        assert!(
            Arc::ptr_eq(batch.table().archive(), context.tables()),
            "the batch belongs to another archive than the render context"
        );
        assert_eq!(
            batch.table().layout().schema_id(),
            self.schema_id,
            "the batch belongs to a table of another schema than the template"
        );
        let timestamps = context.tables().index().metadata().timestamp_index();
        let writers = self
            .slots
            .iter()
            .map(|slot| {
                ValueWriter::bind(
                    slot.encoding,
                    batch.column(slot.column)?,
                    self.options.should_emulate_cpp_plain_floats,
                    context.logtypes(),
                    timestamps,
                )
                .ok_or(JsonError::UnexpectedColumn(slot.column))
            })
            .collect::<Result<_, JsonError>>()?;
        Ok(BoundJsonTemplate {
            template: self,
            batch,
            writers,
        })
    }

    /// Factory function.
    ///
    /// Compiles the template of the records of `layout` whose shape is `events` (the layout's own
    /// shape, or a projected one built from the same schema).
    ///
    /// # Returns
    ///
    /// The template on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`JsonError::Table`] ([`LayoutError::UnknownNode`]) if a key's node is missing from
    ///   `tree`.
    /// * [`JsonError::UnexpectedColumn`] if a column of `events` is outside `layout`.
    pub(crate) fn compile(
        tree: &SchemaTree,
        layout: &TableLayout,
        events: &[ShapeEvent],
        options: JsonOptions,
    ) -> Result<Self, JsonError> {
        let mut compiler = Compiler {
            tree,
            fragments: Vec::new(),
            slots: Vec::new(),
            is_first_member: Vec::new(),
        };
        for event in events {
            match *event {
                ShapeEvent::BeginObject { node_id } => {
                    if let Some(node_id) = node_id {
                        compiler.begin_member(node_id)?;
                    }
                    compiler.fragments.push(b'{');
                    compiler.is_first_member.push(true);
                }
                ShapeEvent::EndObject => {
                    compiler.fragments.push(b'}');
                    compiler.is_first_member.pop();
                }
                ShapeEvent::Column { node_id, column } => {
                    let encoding = layout
                        .columns()
                        .get(column.get() as usize)
                        .ok_or(JsonError::UnexpectedColumn(column))?
                        .encoding;
                    compiler.begin_member(node_id)?;
                    compiler.slot(column, encoding);
                }
                ShapeEvent::Null { node_id } => {
                    compiler.begin_member(node_id)?;
                    compiler.fragments.extend_from_slice(b"null");
                }
                ShapeEvent::EmptyObject { node_id } => {
                    compiler.begin_member(node_id)?;
                    compiler.fragments.extend_from_slice(b"{}");
                }
            }
        }
        Ok(Self {
            schema_id: layout.schema_id(),
            options,
            fragments: compiler.fragments.into_boxed_slice(),
            slots: compiler.slots.into_boxed_slice(),
        })
    }
}

/// A template bound to one batch.
#[derive(Debug)]
pub struct BoundJsonTemplate<'batch> {
    template: &'batch JsonTemplate,
    batch: TableBatch<'batch>,

    /// One writer per slot of the template.
    writers: Box<[ValueWriter<'batch>]>,
}

impl BoundJsonTemplate<'_> {
    /// Appends every selected record of the batch, in row order, to `output`. On failure, the
    /// records appended by this call are removed.
    ///
    /// # Returns
    ///
    /// The number of records appended on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`JsonError::Table`] if a value cannot be decoded.
    /// * [`JsonError::Format`] if a dictionary id or timestamp pattern is invalid.
    /// * [`JsonError::Timestamp`] if a timestamp cannot be formatted with its pattern.
    /// * [`JsonError::BufferOverflow`] if `output` would exceed what its offsets address.
    pub fn write_selected(&self, output: &mut JsonRecordBuffer) -> Result<u32, JsonError> {
        let num_records_before = output.len();
        let mut num_written = 0;
        for (word_base, word) in (0_u32..)
            .step_by(64)
            .zip(self.batch.selection().words().iter().copied())
        {
            let mut remaining = word;
            while 0 != remaining {
                let row = word_base + remaining.trailing_zeros();
                if let Err(error) = output.push_record(|bytes| self.write_row(row, bytes)) {
                    output.truncate_records(num_records_before);
                    return Err(error);
                }
                num_written += 1;
                remaining &= remaining - 1;
            }
        }
        Ok(num_written)
    }

    /// Appends the records of the window rows `rows`, in the given order, to `output` (top-k
    /// winners). On failure, the records appended by this call are removed.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`JsonError::Table`] if a value cannot be decoded.
    /// * [`JsonError::Format`] if a dictionary id or timestamp pattern is invalid.
    /// * [`JsonError::Timestamp`] if a timestamp cannot be formatted with its pattern.
    /// * [`JsonError::BufferOverflow`] if `output` would exceed what its offsets address.
    ///
    /// # Panics
    ///
    /// Panics if a row is not a row of the batch.
    pub fn write_rows(&self, rows: &[u32], output: &mut JsonRecordBuffer) -> Result<(), JsonError> {
        let num_rows = self.batch.num_rows();
        assert!(
            rows.iter().all(|row| *row < num_rows),
            "a row is outside the batch of {num_rows} rows"
        );
        let num_records_before = output.len();
        for row in rows {
            if let Err(error) = output.push_record(|bytes| self.write_row(*row, bytes)) {
                output.truncate_records(num_records_before);
                return Err(error);
            }
        }
        Ok(())
    }

    /// Appends the JSON text of window row `row` to `output`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`ValueWriter::write`]'s return values on failure.
    fn write_row(&self, row: u32, output: &mut Vec<u8>) -> Result<(), JsonError> {
        let fragments = &self.template.fragments;
        let mut fragment_begin = 0;
        for (slot, writer) in self.template.slots.iter().zip(&self.writers) {
            output.extend_from_slice(&fragments[fragment_begin..slot.fragment_end]);
            fragment_begin = slot.fragment_end;
            writer.write(row, output)?;
        }
        output.extend_from_slice(&fragments[fragment_begin..]);
        Ok(())
    }
}

/// One value slot of a template.
#[derive(Clone, Copy, Debug)]
struct Slot {
    /// The end of the fragment bytes written before the value.
    fragment_end: usize,

    column: ColumnIndex,
    encoding: ColumnEncoding,
}

/// The state of compiling one template.
struct Compiler<'tree> {
    tree: &'tree SchemaTree,
    fragments: Vec<u8>,
    slots: Vec<Slot>,

    /// Whether the next member of each open object is its first, innermost last.
    is_first_member: Vec<bool>,
}

impl Compiler<'_> {
    /// Appends the separator (if needed) and the escaped key of the member stored under
    /// `node_id`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`JsonError::Table`] ([`LayoutError::UnknownNode`]) if the node is missing from the tree.
    fn begin_member(&mut self, node_id: NodeId) -> Result<(), JsonError> {
        let key = self
            .tree
            .node(node_id)
            .ok_or(TableError::Layout(LayoutError::UnknownNode(node_id)))?
            .key;
        if let Some(is_first) = self.is_first_member.last_mut() {
            if !*is_first {
                self.fragments.push(b',');
            }
            *is_first = false;
        }
        self.fragments.push(b'"');
        escape_into(key, &mut self.fragments);
        self.fragments.extend_from_slice(b"\":");
        Ok(())
    }

    /// Appends a value slot for `column`; string values get their quotes as fragments.
    fn slot(&mut self, column: ColumnIndex, encoding: ColumnEncoding) {
        let is_string = matches!(
            encoding,
            ColumnEncoding::ClpString | ColumnEncoding::VariableString
        );
        if is_string {
            self.fragments.push(b'"');
        }
        self.slots.push(Slot {
            fragment_end: self.fragments.len(),
            column,
            encoding,
        });
        if is_string {
            self.fragments.push(b'"');
        }
    }
}
