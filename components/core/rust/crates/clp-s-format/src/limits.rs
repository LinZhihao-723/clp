//! Limits on allocations driven by sizes and counts read from an archive, and open options.

/// Bounds on allocations driven by sizes and counts read from the archive; violations are
/// [`crate::FormatError::LimitExceeded`], never corruption.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReaderLimits {
    /// Maximum size of the metadata section, compressed.
    pub max_metadata_section_size: u64,

    /// Maximum decoded size of one section (metadata, schema layer, one dictionary).
    pub max_section_decoded_size: u64,

    /// Maximum decoded size of one packed stream.
    pub max_packed_stream_size: u64,

    /// Maximum number of entries in one dictionary.
    pub max_dictionary_entries: u64,

    /// Maximum depth of the schema tree.
    pub max_schema_tree_depth: u32,
}

impl Default for ReaderLimits {
    fn default() -> Self {
        const GIB: u64 = 1024 * 1024 * 1024;
        Self {
            max_metadata_section_size: GIB,
            max_section_decoded_size: 4 * GIB,
            max_packed_stream_size: 16 * GIB,
            max_dictionary_entries: 1 << 32,
            max_schema_tree_depth: 2048,
        }
    }
}

/// Options that shape an archive open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenOptions {
    pub limits: ReaderLimits,

    /// Size of the speculative first read at offset 0 (default 64 KiB).
    pub prefix_fetch_size: u64,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            limits: ReaderLimits::default(),
            prefix_fetch_size: 64 * 1024,
        }
    }
}
