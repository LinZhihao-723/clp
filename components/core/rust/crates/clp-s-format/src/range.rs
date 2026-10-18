//! Byte ranges within an archive file.

/// Absolute byte range `[begin, end)` within the archive file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ByteRange {
    pub begin: u64,
    pub end: u64,
}

impl ByteRange {
    /// # Returns
    ///
    /// The number of bytes in the range (0 if `end <= begin`).
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.end.saturating_sub(self.begin)
    }

    /// # Returns
    ///
    /// Whether the range holds no bytes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.end <= self.begin
    }
}
