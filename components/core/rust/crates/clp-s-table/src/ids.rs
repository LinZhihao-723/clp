//! Identifiers of the table layer.

/// Position of a column in its table's buffer order: a column's only identity inside a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ColumnIndex(u32);

impl ColumnIndex {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The index `value`.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// # Returns
    ///
    /// The index as an integer.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Stable identity of a record within its archive: `log_event_idx` if the archive records log
/// order, else `first_record_ordinal + row`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RecordId(i64);

impl RecordId {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The id `value`.
    #[must_use]
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    /// # Returns
    ///
    /// The id as an integer.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}
