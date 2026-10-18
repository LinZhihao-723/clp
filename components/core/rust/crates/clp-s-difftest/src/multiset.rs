//! Multiset comparison of canonical records.

use std::collections::HashMap;

use sha2::Digest;
use sha2::Sha256;

use crate::canonical::canonical_record;

/// The size beyond which [`MultisetDiff::render`] truncates a record.
pub const MAX_RENDERED_RECORD_SIZE: usize = 400;

/// A multiset of canonical records.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecordMultiset {
    counts: HashMap<Vec<u8>, u64>,
}

impl RecordMultiset {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The multiset of the canonical forms of the non-empty lines of `data`.
    #[must_use]
    pub fn from_jsonl(data: &[u8]) -> Self {
        let mut multiset = Self::default();
        for line in data.split(|byte| *byte == b'\n') {
            if !line.is_empty() {
                multiset.insert(canonical_record(line));
            }
        }
        multiset
    }

    /// Adds one canonical record.
    pub fn insert(&mut self, canonical: Vec<u8>) {
        *self.counts.entry(canonical).or_default() += 1;
    }

    /// # Returns
    ///
    /// The number of records, counting repeats.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.counts.values().sum()
    }

    /// # Returns
    ///
    /// Whether the multiset is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }

    /// # Returns
    ///
    /// How many times `canonical` occurs.
    #[must_use]
    pub fn count(&self, canonical: &[u8]) -> u64 {
        self.counts.get(canonical).copied().unwrap_or_default()
    }

    /// Compares `self` (the expected records) with `actual`.
    ///
    /// # Returns
    ///
    /// The records missing from `actual` and the records only `actual` has, with multiplicities,
    /// each sorted by canonical bytes.
    #[must_use]
    pub fn diff(&self, actual: &Self) -> MultisetDiff {
        let mut missing: Vec<(Vec<u8>, u64)> = self
            .counts
            .iter()
            .filter_map(|(record, count)| {
                let actual_count = actual.count(record);
                (*count > actual_count).then(|| (record.clone(), count - actual_count))
            })
            .collect();
        let mut unexpected: Vec<(Vec<u8>, u64)> = actual
            .counts
            .iter()
            .filter_map(|(record, count)| {
                let expected_count = self.count(record);
                (*count > expected_count).then(|| (record.clone(), count - expected_count))
            })
            .collect();
        missing.sort_unstable();
        unexpected.sort_unstable();
        MultisetDiff {
            missing,
            unexpected,
        }
    }
}

/// An order-independent fingerprint of a multiset of canonical records.
///
/// It is the count and the wrapping sum of the leading 128 bits of each record's SHA-256, so it
/// holds no record: a multi-million-record reference result costs 24 bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MultisetFingerprint {
    pub count: u64,
    pub sum: u128,
}

impl MultisetFingerprint {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The fingerprint of the canonical forms of the non-empty lines of `data`.
    #[must_use]
    pub fn of_jsonl(data: &[u8]) -> Self {
        let mut fingerprint = Self::default();
        for line in data.split(|byte| *byte == b'\n') {
            if !line.is_empty() {
                fingerprint.insert(&canonical_record(line));
            }
        }
        fingerprint
    }

    /// Adds one canonical record.
    pub fn insert(&mut self, canonical: &[u8]) {
        let digest = Sha256::digest(canonical);
        let mut leading = [0_u8; 16];
        leading.copy_from_slice(&digest[..16]);
        self.count += 1;
        self.sum = self.sum.wrapping_add(u128::from_le_bytes(leading));
    }
}

/// The difference between an expected and an actual multiset.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MultisetDiff {
    /// Canonical records (with their missing multiplicity) the actual output lacks.
    pub missing: Vec<(Vec<u8>, u64)>,

    /// Canonical records (with their extra multiplicity) only the actual output has.
    pub unexpected: Vec<(Vec<u8>, u64)>,
}

impl MultisetDiff {
    /// # Returns
    ///
    /// Whether the two multisets are equal.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.missing.is_empty() && self.unexpected.is_empty()
    }

    /// # Returns
    ///
    /// The number of missing records, counting repeats.
    #[must_use]
    pub fn num_missing(&self) -> u64 {
        self.missing.iter().map(|(_, count)| count).sum()
    }

    /// # Returns
    ///
    /// The number of unexpected records, counting repeats.
    #[must_use]
    pub fn num_unexpected(&self) -> u64 {
        self.unexpected.iter().map(|(_, count)| count).sum()
    }

    /// Renders the difference for a human: the counts, then at most `max_records` records of
    /// each side (each truncated to [`MAX_RENDERED_RECORD_SIZE`] bytes).
    ///
    /// # Returns
    ///
    /// The rendered text, one line per entry.
    #[must_use]
    pub fn render(&self, max_records: usize) -> String {
        let mut lines = vec![format!(
            "{} missing ({} distinct), {} unexpected ({} distinct)",
            self.num_missing(),
            self.missing.len(),
            self.num_unexpected(),
            self.unexpected.len()
        )];
        for (label, records) in [("-", &self.missing), ("+", &self.unexpected)] {
            for (record, count) in records.iter().take(max_records) {
                lines.push(format!("  {label} x{count} {}", render_record(record)));
            }
            if records.len() > max_records {
                lines.push(format!(
                    "  {label} ... {} more distinct records",
                    records.len() - max_records
                ));
            }
        }
        lines.join("\n")
    }
}

/// # Returns
///
/// `record` as lossy UTF-8, truncated to [`MAX_RENDERED_RECORD_SIZE`] bytes.
#[must_use]
pub fn render_record(record: &[u8]) -> String {
    if record.len() <= MAX_RENDERED_RECORD_SIZE {
        return String::from_utf8_lossy(record).into_owned();
    }
    format!(
        "{}... ({} bytes)",
        String::from_utf8_lossy(&record[..MAX_RENDERED_RECORD_SIZE]),
        record.len()
    )
}

#[cfg(test)]
mod tests {
    use super::MAX_RENDERED_RECORD_SIZE;
    use super::MultisetDiff;
    use super::MultisetFingerprint;
    use super::RecordMultiset;
    use super::render_record;

    #[test]
    fn fingerprints_ignore_order_and_key_order_but_not_multiplicity() {
        let fingerprint = MultisetFingerprint::of_jsonl(b"{\"b\":1,\"a\":2}\n[1]\n\n");
        assert_eq!(fingerprint.count, 2);
        assert_eq!(
            fingerprint,
            MultisetFingerprint::of_jsonl(b"[1]\n{\"a\":2,\"b\":1}\n")
        );
        assert_ne!(
            fingerprint,
            MultisetFingerprint::of_jsonl(b"[1]\n[1]\n{\"a\":2,\"b\":1}\n")
        );
        assert_ne!(
            fingerprint,
            MultisetFingerprint::of_jsonl(b"[2]\n{\"a\":2,\"b\":1}\n")
        );
        assert_eq!(
            MultisetFingerprint::of_jsonl(b""),
            MultisetFingerprint::default()
        );
    }

    #[test]
    fn from_jsonl_counts_canonical_records() {
        let multiset = RecordMultiset::from_jsonl(b"{\"b\":1,\"a\":2}\n\n{\"a\":2,\"b\":1}\nbad\n");
        assert_eq!(multiset.len(), 3);
        assert_eq!(multiset.count(br#"{"a":2,"b":1}"#), 2);
        assert_eq!(multiset.count(b"INVALID:bad"), 1);
        assert!(!multiset.is_empty());
        assert_eq!(
            RecordMultiset::from_jsonl(b"\n\n"),
            RecordMultiset::default()
        );
    }

    #[test]
    fn diff_reports_multiplicities_on_both_sides() {
        let expected = RecordMultiset::from_jsonl(b"[1]\n[1]\n[2]\n");
        let actual = RecordMultiset::from_jsonl(b"[1]\n[3]\n[3]\n");
        assert_eq!(
            expected.diff(&actual),
            MultisetDiff {
                missing: vec![(b"[1]".to_vec(), 1), (b"[2]".to_vec(), 1)],
                unexpected: vec![(b"[3]".to_vec(), 2)],
            }
        );
        assert_eq!(expected.diff(&expected), MultisetDiff::default());
    }

    #[test]
    fn render_shows_counts_and_at_most_max_records() {
        let expected = RecordMultiset::from_jsonl(b"[1]\n[2]\n[3]\n");
        let diff = expected.diff(&RecordMultiset::default());
        let text = diff.render(2);
        assert_eq!(
            text,
            "3 missing (3 distinct), 0 unexpected (0 distinct)\n  - x1 [1]\n  - x1 [2]\n  - ... 1 \
             more distinct records"
        );
    }

    #[test]
    fn long_records_are_truncated_when_rendered() {
        let record = vec![b'a'; MAX_RENDERED_RECORD_SIZE + 10];
        let rendered = render_record(&record);
        assert!(
            rendered.ends_with(&format!("... ({} bytes)", record.len())),
            "{rendered}"
        );
        assert_eq!(render_record(b"[1]"), "[1]");
    }
}
