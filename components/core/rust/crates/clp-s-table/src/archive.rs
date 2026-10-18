//! Table-layer state of one archive.

use std::sync::Arc;
use std::sync::OnceLock;

use clp_s_format::ArchiveDictionaries;
use clp_s_format::ArchiveIndex;
use clp_s_format::TableOrdinal;

use crate::LayoutError;
use crate::TableLayout;

/// Table-layer state of one archive: lazily compiled layouts and the shared dictionaries.
#[derive(Debug)]
pub struct ArchiveTables {
    index: Arc<ArchiveIndex>,
    dictionaries: Arc<ArchiveDictionaries>,

    /// One cell per table, in directory order.
    layouts: Box<[OnceLock<Arc<TableLayout>>]>,
}

impl ArchiveTables {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The table layer of the archive with `index`, reading dictionaries from `dictionaries`.
    #[must_use]
    pub fn new(index: Arc<ArchiveIndex>, dictionaries: Arc<ArchiveDictionaries>) -> Self {
        let layouts = index
            .tables()
            .tables()
            .iter()
            .map(|_| OnceLock::new())
            .collect();
        Self {
            index,
            dictionaries,
            layouts,
        }
    }

    /// # Returns
    ///
    /// The archive index.
    #[must_use]
    pub const fn index(&self) -> &Arc<ArchiveIndex> {
        &self.index
    }

    /// # Returns
    ///
    /// The archive's dictionaries.
    #[must_use]
    pub const fn dictionaries(&self) -> &Arc<ArchiveDictionaries> {
        &self.dictionaries
    }

    /// Returns the layout of a table, compiling and validating it on first use; a failed
    /// compilation is reported on every call and never cached as a layout.
    ///
    /// # Returns
    ///
    /// The layout on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`LayoutError::UnknownTable`] if `table` does not exist.
    /// * Forwards [`TableLayout::compile`]'s return values on failure.
    pub fn layout(&self, table: TableOrdinal) -> Result<Arc<TableLayout>, LayoutError> {
        let (Some(info), Some(cell)) = (
            self.index.tables().table(table),
            self.layouts.get(table.get() as usize),
        ) else {
            return Err(LayoutError::UnknownTable(table));
        };
        if let Some(layout) = cell.get() {
            return Ok(Arc::clone(layout));
        }
        let layout = Arc::new(TableLayout::compile(&self.index, info.schema_id)?);
        Ok(Arc::clone(cell.get_or_init(|| layout)))
    }
}
