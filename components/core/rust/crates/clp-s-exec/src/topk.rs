//! The session-wide top-k operator and its key.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::num::NonZeroU32;

use clp_s_table::RecordId;

use crate::ArchiveId;
use crate::OwnedBatch;
use crate::ReadError;

/// The total order of [`crate::ResultOrder::LatestFirst`]; sinks use the same type.
///
/// Timestamp descending with missing timestamps last (DV-34), then archive id ascending, then
/// record id ascending: the api-server's read-time sort (`components/api-server/src/client.rs`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LatestFirstKey {
    /// The authoritative timestamp in epoch milliseconds (floor of the nanoseconds, DV-27).
    pub timestamp_ms: Option<i64>,

    pub archive_id: ArchiveId,
    pub record_id: RecordId,
}

impl LatestFirstKey {
    /// # Returns
    ///
    /// The key timestamp of an authoritative timestamp of `epoch_ns` nanoseconds: its epoch
    /// milliseconds, rounded toward negative infinity (DV-27).
    #[must_use]
    pub const fn timestamp_ms_of(epoch_ns: i64) -> i64 {
        epoch_ns.div_euclid(1_000_000)
    }

    /// # Returns
    ///
    /// The key as a borrowed view.
    const fn view(&self) -> KeyView<'_> {
        KeyView {
            timestamp_ms: self.timestamp_ms,
            archive_id: &self.archive_id,
            record_id: self.record_id,
        }
    }
}

impl Ord for LatestFirstKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.view().cmp(&other.view())
    }
}

impl PartialOrd for LatestFirstKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The latest-first top-k of the records offered so far: the `limit` smallest
/// [`LatestFirstKey`]s, each with its owned record.
#[derive(Debug)]
pub struct TopK {
    limit: usize,

    /// A max-heap: its top is the worst kept record, the first to be evicted.
    heap: BinaryHeap<Entry>,
}

impl TopK {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// An empty top-`limit`.
    #[must_use]
    pub fn new(limit: NonZeroU32) -> Self {
        Self {
            limit: usize::try_from(limit.get()).unwrap_or(usize::MAX),
            heap: BinaryHeap::new(),
        }
    }

    /// # Returns
    ///
    /// Whether a record with the key (`timestamp_ms`, `archive_id`, `record_id`) would be kept if
    /// offered now.
    #[must_use]
    pub fn admits(
        &self,
        timestamp_ms: Option<i64>,
        archive_id: &ArchiveId,
        record_id: RecordId,
    ) -> bool {
        if self.heap.len() < self.limit {
            return true;
        }
        let candidate = KeyView {
            timestamp_ms,
            archive_id,
            record_id,
        };
        self.heap
            .peek()
            .is_some_and(|worst| Ordering::Less == candidate.cmp(&worst.record.key.view()))
    }

    /// Offers a record, keeping it if its key is among the `limit` smallest so far; an evicted
    /// record is dropped.
    pub fn offer(&mut self, record: TopKRecord) {
        if self.heap.len() < self.limit {
            self.heap.push(Entry { record });
            return;
        }
        if let Some(mut worst) = self.heap.peek_mut()
            && record.key < worst.record.key
        {
            *worst = Entry { record };
        }
    }

    /// Decides, before an archive is read, whether reading it can change the result.
    ///
    /// # Returns
    ///
    /// Whether no record of the archive `archive_id`, whose authoritative timestamps are bounded
    /// by `bound`, can be kept.
    #[must_use]
    pub fn excludes_archive(&self, archive_id: &ArchiveId, bound: TimestampBound) -> bool {
        if self.heap.len() < self.limit {
            return false;
        }
        let Some(worst) = self.heap.peek() else {
            return false;
        };
        let Some(worst_timestamp) = worst.record.key.timestamp_ms else {
            return false;
        };
        match bound {
            TimestampBound::Unknown => false,
            TimestampBound::Absent => true,
            TimestampBound::AtMostMs(bound) => {
                bound < worst_timestamp
                    || (bound == worst_timestamp && *archive_id > worst.record.key.archive_id)
            }
        }
    }

    /// Consumes the operator.
    ///
    /// # Returns
    ///
    /// The kept records sorted by key, as one owned batch, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`OwnedBatch::push`]'s return values on failure.
    pub fn into_batch(self) -> Result<OwnedBatch, ReadError> {
        let mut batch = OwnedBatch::default();
        for Entry { record } in self.heap.into_sorted_vec() {
            batch.push(
                record.key.archive_id,
                record.key.record_id,
                record.timestamp_ns,
                record.log_event_idx,
                record.json.as_deref(),
            )?;
        }
        Ok(batch)
    }
}

/// What is known about the authoritative timestamps of an archive before it is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimestampBound {
    /// The archive's timestamp range is not known.
    Unknown,

    /// The archive has no authoritative timestamp: every record's key timestamp is `None`.
    Absent,

    /// Every authoritative timestamp of the archive is at most this many epoch milliseconds.
    AtMostMs(i64),
}

/// One record offered to a [`TopK`].
#[derive(Debug)]
pub struct TopKRecord {
    pub key: LatestFirstKey,
    pub timestamp_ns: Option<i64>,
    pub log_event_idx: Option<i64>,

    /// The rendered record without its newline, if JSON is needed.
    pub json: Option<Box<[u8]>>,
}

/// A [`LatestFirstKey`] whose archive id is borrowed, so a candidate needs no owned id.
#[derive(Clone, Copy, Debug)]
struct KeyView<'key> {
    timestamp_ms: Option<i64>,
    archive_id: &'key ArchiveId,
    record_id: RecordId,
}

impl KeyView<'_> {
    /// # Returns
    ///
    /// The [`LatestFirstKey`] order of `self` relative to `other`.
    fn cmp(&self, other: &Self) -> Ordering {
        let timestamp_order = match (self.timestamp_ms, other.timestamp_ms) {
            (Some(timestamp), Some(other_timestamp)) => other_timestamp.cmp(&timestamp),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        };
        timestamp_order
            .then_with(|| self.archive_id.cmp(other.archive_id))
            .then_with(|| self.record_id.cmp(&other.record_id))
    }
}

/// A kept record, ordered by its key alone.
#[derive(Debug)]
struct Entry {
    record: TopKRecord,
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.record.key.cmp(&other.record.key)
    }
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.record.key == other.record.key
    }
}

impl Eq for Entry {}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use clp_s_table::RecordId;

    use super::LatestFirstKey;
    use super::TimestampBound;
    use super::TopK;
    use super::TopKRecord;
    use crate::ArchiveId;
    use crate::OutputRequirements;
    use crate::RecordBatch;

    /// # Returns
    ///
    /// The key of (`timestamp_ms`, `archive_id`, `record_id`).
    fn key(timestamp_ms: Option<i64>, archive_id: &str, record_id: i64) -> LatestFirstKey {
        LatestFirstKey {
            timestamp_ms,
            archive_id: ArchiveId::from(archive_id),
            record_id: RecordId::new(record_id),
        }
    }

    /// # Returns
    ///
    /// A record with `key` whose JSON names its key.
    fn record(key: LatestFirstKey) -> TopKRecord {
        let json = format!(
            "{{\"archive\":\"{}\",\"record\":{}}}",
            key.archive_id,
            key.record_id.get()
        );
        TopKRecord {
            timestamp_ns: key.timestamp_ms.map(|timestamp| timestamp * 1_000_000 + 7),
            log_event_idx: Some(key.record_id.get()),
            json: Some(json.into_bytes().into_boxed_slice()),
            key,
        }
    }

    /// # Returns
    ///
    /// A top-`limit`.
    fn top_k(limit: u32) -> TopK {
        TopK::new(NonZeroU32::new(limit).expect("the limit is non-zero"))
    }

    /// # Returns
    ///
    /// The keys `top_k` keeps, in key order.
    fn kept_keys(top_k: TopK) -> Vec<LatestFirstKey> {
        let batch = top_k.into_batch().expect("the winners fit one batch");
        let records = RecordBatch::from_gathered(
            &batch,
            OutputRequirements::json()
                .with_timestamps()
                .with_log_event_indices(),
        );
        (0..records.num_records())
            .map(|index| LatestFirstKey {
                timestamp_ms: records.timestamps_ns().expect("timestamps are exposed")[index]
                    .map(|timestamp| timestamp.div_euclid(1_000_000)),
                archive_id: records.archive_id(index).clone(),
                record_id: records.record_ids()[index],
            })
            .collect()
    }

    #[test]
    fn key_timestamps_floor_nanoseconds_to_milliseconds() {
        for (epoch_ns, expected_ms) in [
            (0, 0),
            (999_999, 0),
            (1_000_000, 1),
            (1_735_694_600_123_999_999, 1_735_694_600_123),
            (-1, -1),
            (-1_000_000, -1),
            (-1_000_001, -2),
        ] {
            assert_eq!(
                LatestFirstKey::timestamp_ms_of(epoch_ns),
                expected_ms,
                "{epoch_ns}"
            );
        }
    }

    #[test]
    fn keys_order_latest_first_then_by_archive_and_record() {
        let mut keys = vec![
            key(None, "a", 0),
            key(Some(5), "b", 1),
            key(Some(-3), "a", 0),
            key(Some(5), "a", 7),
            key(Some(5), "a", 2),
            key(Some(9), "z", 0),
            key(None, "a", -1),
        ];
        keys.sort();
        assert_eq!(
            keys,
            vec![
                key(Some(9), "z", 0),
                key(Some(5), "a", 2),
                key(Some(5), "a", 7),
                key(Some(5), "b", 1),
                key(Some(-3), "a", 0),
                key(None, "a", -1),
                key(None, "a", 0),
            ]
        );
    }

    #[test]
    fn the_top_k_keeps_the_smallest_keys_in_any_offer_order() {
        let keys = [
            key(Some(10), "a", 0),
            key(None, "a", 1),
            key(Some(30), "b", 2),
            key(Some(20), "a", 3),
            key(Some(30), "a", 4),
            key(Some(-5), "c", 5),
            key(None, "0", 6),
            key(Some(30), "a", 1),
        ];
        let mut expected = keys.to_vec();
        expected.sort();
        for limit in 1..=10_u32 {
            for rotation in 0..keys.len() {
                let mut top_k = top_k(limit);
                for offered in keys.iter().cycle().skip(rotation).take(keys.len()) {
                    top_k.offer(record(offered.clone()));
                }
                let num_kept = keys.len().min(limit as usize);
                assert_eq!(
                    kept_keys(top_k),
                    expected[..num_kept],
                    "limit {limit}, rotation {rotation}"
                );
            }
        }
    }

    #[test]
    fn ties_on_the_timestamp_break_by_archive_then_record() {
        let mut top_k = top_k(3);
        for offered in [
            key(Some(7), "b", 0),
            key(Some(7), "a", 9),
            key(Some(7), "b", 1),
            key(Some(7), "a", 3),
            key(Some(7), "c", 0),
        ] {
            top_k.offer(record(offered));
        }
        assert_eq!(
            kept_keys(top_k),
            vec![
                key(Some(7), "a", 3),
                key(Some(7), "a", 9),
                key(Some(7), "b", 0)
            ]
        );
    }

    #[test]
    fn missing_timestamps_sort_after_every_timestamp() {
        let mut top_k = top_k(3);
        for offered in [
            key(None, "a", 0),
            key(None, "a", 1),
            key(Some(-9_000_000_000_000), "z", 5),
            key(None, "0", 2),
        ] {
            top_k.offer(record(offered));
        }
        assert_eq!(
            kept_keys(top_k),
            vec![
                key(Some(-9_000_000_000_000), "z", 5),
                key(None, "0", 2),
                key(None, "a", 0)
            ]
        );
    }

    #[test]
    fn admission_matches_what_an_offer_would_keep() {
        let mut top_k = top_k(2);
        let archive = ArchiveId::from("b");
        assert!(top_k.admits(None, &archive, RecordId::new(0)));
        top_k.offer(record(key(Some(10), "b", 4)));
        top_k.offer(record(key(Some(20), "b", 1)));
        for (timestamp_ms, archive_id, record_id, is_admitted) in [
            (Some(11), "b", 0, true),
            (Some(10), "a", 99, true),
            (Some(10), "b", 3, true),
            (Some(10), "b", 4, false),
            (Some(10), "b", 5, false),
            (Some(10), "c", 0, false),
            (Some(9), "a", 0, false),
            (None, "a", 0, false),
        ] {
            assert_eq!(
                top_k.admits(
                    timestamp_ms,
                    &ArchiveId::from(archive_id),
                    RecordId::new(record_id)
                ),
                is_admitted,
                "({timestamp_ms:?}, {archive_id}, {record_id})"
            );
        }
    }

    #[test]
    fn archives_are_excluded_only_when_none_of_their_records_can_win() {
        let mut top_k = top_k(2);
        let archive = ArchiveId::from("m");
        assert!(!top_k.excludes_archive(&archive, TimestampBound::Absent));
        top_k.offer(record(key(Some(50), "m", 0)));
        assert!(
            !top_k.excludes_archive(&archive, TimestampBound::AtMostMs(0)),
            "an unfilled top-k admits anything"
        );
        top_k.offer(record(key(Some(40), "m", 1)));
        for (archive_id, bound, is_excluded) in [
            ("x", TimestampBound::Unknown, false),
            ("x", TimestampBound::Absent, true),
            ("x", TimestampBound::AtMostMs(39), true),
            ("x", TimestampBound::AtMostMs(40), true),
            ("a", TimestampBound::AtMostMs(40), false),
            ("x", TimestampBound::AtMostMs(41), false),
        ] {
            assert_eq!(
                top_k.excludes_archive(&ArchiveId::from(archive_id), bound),
                is_excluded,
                "{archive_id} {bound:?}"
            );
        }

        let mut untimed = super::TopK::new(NonZeroU32::MIN);
        untimed.offer(record(key(None, "m", 0)));
        assert!(!untimed.excludes_archive(&ArchiveId::from("z"), TimestampBound::Absent));
        assert!(!untimed.excludes_archive(&ArchiveId::from("z"), TimestampBound::AtMostMs(0)));
    }

    #[test]
    fn winners_keep_their_records() {
        let mut top_k = top_k(2);
        top_k.offer(record(key(Some(1), "a", 1)));
        top_k.offer(record(key(Some(2), "b", 2)));
        let batch = top_k.into_batch().expect("the winners fit one batch");
        let records = RecordBatch::from_gathered(&batch, OutputRequirements::json());
        let json = records.json().expect("JSON is exposed");
        assert_eq!(
            json.as_jsonl(),
            b"{\"archive\":\"b\",\"record\":2}\n{\"archive\":\"a\",\"record\":1}\n"
        );
        assert_eq!(records.timestamps_ns(), None);
        assert_eq!(records.log_event_indices(), None);
        assert_eq!(records.archive_id(0).as_str(), "b");
        assert_eq!(records.archive_id(1).as_str(), "a");
    }
}
