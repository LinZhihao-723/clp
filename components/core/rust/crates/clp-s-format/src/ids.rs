//! Identifier newtypes of the archive format.

/// A schema-tree (MPT) node id.
///
/// Ids are positions in the schema tree; an id obtained from [`crate::SchemaTree`] is valid for
/// that tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(u32);

impl NodeId {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The id `value`.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// # Returns
    ///
    /// The id as an integer.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A schema id as stored in the schema map and the table metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SchemaId(i32);

impl SchemaId {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The id `value`.
    #[must_use]
    pub const fn new(value: i32) -> Self {
        Self(value)
    }

    /// # Returns
    ///
    /// The id as an integer.
    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }
}

/// Position of an ERT in the table directory (writer order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TableOrdinal(u32);

impl TableOrdinal {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The ordinal `value`.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// # Returns
    ///
    /// The ordinal as an integer.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Index of a packed stream (one zstd frame of consecutive ERTs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StreamId(u32);

impl StreamId {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The id `value`.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// # Returns
    ///
    /// The id as an integer.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// An entry id of the variable dictionary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VariableId(u64);

impl VariableId {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The id `value`.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// # Returns
    ///
    /// The id as an integer.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// An entry id of the logtype or array dictionary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LogTypeId(u64);

impl LogTypeId {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The id `value`.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// # Returns
    ///
    /// The id as an integer.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The id of a timestamp pattern in the timestamp dictionary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TimestampPatternId(u64);

impl TimestampPatternId {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The id `value`.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// # Returns
    ///
    /// The id as an integer.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}
