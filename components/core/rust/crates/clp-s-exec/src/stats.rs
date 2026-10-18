//! Scan statistics.

/// Mergeable counters of one scan or session; returned, never written to global state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanStats {
    pub num_tables_planned: u64,
    pub num_tables_scanned: u64,
    pub num_rows_scanned: u64,
    pub num_records_selected: u64,

    /// The selected records rendered as JSON; under [`crate::ResultOrder::LatestFirst`], only
    /// those the top-k kept when they were offered.
    pub num_records_rendered: u64,

    pub num_streams_decoded: u64,
    pub num_decoded_bytes: u64,
    pub num_bytes_read: u64,
}

impl ScanStats {
    /// Adds every counter of `other` to `self`.
    pub const fn merge(&mut self, other: &Self) {
        self.num_tables_planned += other.num_tables_planned;
        self.num_tables_scanned += other.num_tables_scanned;
        self.num_rows_scanned += other.num_rows_scanned;
        self.num_records_selected += other.num_records_selected;
        self.num_records_rendered += other.num_records_rendered;
        self.num_streams_decoded += other.num_streams_decoded;
        self.num_decoded_bytes += other.num_decoded_bytes;
        self.num_bytes_read += other.num_bytes_read;
    }
}
