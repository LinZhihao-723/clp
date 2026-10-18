//! Record views: one record of a batch walked in its table's shape.

use crate::LayoutError;
use crate::ShapeEvent;
use crate::TableBatch;
use crate::TableError;
use crate::ValueRef;

/// Receives the structure and leaves of one record from [`RecordView::walk`].
pub trait RecordVisitor {
    /// The error the visitor fails with; walking also fails with the table errors of resolving
    /// leaf values.
    type Error: From<TableError>;

    /// Opens an object.
    ///
    /// # Parameters
    ///
    /// * `key` - The key the object is stored under, or `None` for the record root.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Self::Error`] if the visitor stops the walk.
    fn begin_object(&mut self, key: Option<&[u8]>) -> Result<(), Self::Error>;

    /// Closes the most recently opened object.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Self::Error`] if the visitor stops the walk.
    fn end_object(&mut self) -> Result<(), Self::Error>;

    /// Receives one leaf of the current object.
    ///
    /// # Parameters
    ///
    /// * `key` - The leaf's key.
    /// * `value` - The leaf's value.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Self::Error`] if the visitor stops the walk.
    fn value(&mut self, key: &[u8], value: ValueRef<'_>) -> Result<(), Self::Error>;
}

/// One record of a [`TableBatch`].
#[derive(Clone, Copy, Debug)]
pub struct RecordView<'batch> {
    batch: TableBatch<'batch>,
    row: u32,
}

impl<'batch> RecordView<'batch> {
    /// # Returns
    ///
    /// The batch the record belongs to.
    #[must_use]
    pub const fn batch(&self) -> &TableBatch<'batch> {
        &self.batch
    }

    /// # Returns
    ///
    /// The window row of the record.
    #[must_use]
    pub const fn row(&self) -> u32 {
        self.row
    }

    /// Walks the record in its table's [`crate::RecordShape`], in C++ key order, calling
    /// `visitor` with every object boundary and every leaf. Every column of the shape must be
    /// decoded in the batch.
    ///
    /// # Type Parameters
    ///
    /// * `VisitorType` - The visitor receiving the record.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`TableError::Layout`] ([`LayoutError::UnknownNode`]) if a key's node is missing from the
    ///   schema tree.
    /// * Forwards [`TableBatch::column`]'s return values on failure.
    /// * Forwards [`crate::ColumnBatch::value`]'s return values on failure.
    /// * Forwards [`RecordVisitor::begin_object`]'s return values on failure.
    /// * Forwards [`RecordVisitor::end_object`]'s return values on failure.
    /// * Forwards [`RecordVisitor::value`]'s return values on failure.
    pub fn walk<VisitorType: RecordVisitor>(
        &self,
        visitor: &mut VisitorType,
    ) -> Result<(), VisitorType::Error> {
        let tree = self.batch.table().archive().index().schema_tree();
        let key = |node_id| {
            tree.node(node_id)
                .map(|node| node.key)
                .ok_or(TableError::Layout(LayoutError::UnknownNode(node_id)))
        };
        for event in self.batch.table().layout().shape().events() {
            match *event {
                ShapeEvent::BeginObject { node_id: None } => visitor.begin_object(None)?,
                ShapeEvent::BeginObject {
                    node_id: Some(node_id),
                } => visitor.begin_object(Some(key(node_id)?))?,
                ShapeEvent::EndObject => visitor.end_object()?,
                ShapeEvent::Column { node_id, column } => {
                    let value = self.batch.column(column)?.value(self.row)?;
                    visitor.value(key(node_id)?, value)?;
                }
                ShapeEvent::Null { node_id } => visitor.value(key(node_id)?, ValueRef::Null)?,
                ShapeEvent::EmptyObject { node_id } => {
                    visitor.value(key(node_id)?, ValueRef::EmptyObject)?;
                }
            }
        }
        Ok(())
    }

    /// Factory function.
    ///
    /// # Returns
    ///
    /// The record of window row `row` of `batch`.
    pub(crate) const fn new(batch: TableBatch<'batch>, row: u32) -> Self {
        Self { batch, row }
    }
}
