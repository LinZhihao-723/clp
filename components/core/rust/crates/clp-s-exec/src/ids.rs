//! Archive identifiers.

use std::sync::Arc;

/// The id of an archive, as the caller names it (for a file, its last path component, like C++).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArchiveId(Arc<str>);

impl ArchiveId {
    /// # Returns
    ///
    /// The id as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ArchiveId {
    fn from(value: &str) -> Self {
        Self(Arc::from(value))
    }
}

impl From<String> for ArchiveId {
    fn from(value: String) -> Self {
        Self(Arc::from(value))
    }
}

impl std::fmt::Display for ArchiveId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}
