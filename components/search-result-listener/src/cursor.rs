//! The per-archive cursor that lets exactly one attempt of a query task emit each result of an
//! archive that the task searches.

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use clp_rust_utils::types::ArchiveId;

/// The outcome of claiming a result index on a [`ArchiveCursor`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Claim {
    /// The caller is the first to deliver the result, so it must emit it.
    Claimed,

    /// An attempt of the archive's task already emitted the result, so the caller must drop it.
    Duplicate,
}

/// Tracks which results of one archive have been emitted across every attempt of the query task
/// that searches it.
#[derive(Debug)]
pub struct ArchiveCursor {
    next_result_index: AtomicU64,
    archive_id: ArchiveId,
}

impl ArchiveCursor {
    #[must_use]
    pub const fn new(archive_id: ArchiveId) -> Self {
        Self {
            next_result_index: AtomicU64::new(0),
            archive_id,
        }
    }

    #[must_use]
    pub const fn archive_id(&self) -> ArchiveId {
        self.archive_id
    }

    /// Claims the result at `result_index` for the calling connection.
    ///
    /// # Returns
    ///
    /// * [`Claim::Claimed`] if the caller is the first to deliver the result.
    /// * [`Claim::Duplicate`] if the result has already been emitted.
    ///
    /// # Panics
    ///
    /// Panics if `result_index` is above the next unclaimed result index. Every connection checks
    /// that its results arrive in index order from 0, so each index below `result_index` has
    /// already been claimed.
    pub fn claim(&self, result_index: u64) -> Claim {
        // The swap from `u64::MAX` to 0 would need the cursor to reach `u64::MAX` first, so
        // wrapping can never make a bogus claim.
        match self.next_result_index.compare_exchange(
            result_index,
            result_index.wrapping_add(1),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => Claim::Claimed,
            Err(current) => {
                assert!(
                    current > result_index,
                    "result index {result_index} is above the next unclaimed result index \
                     {current}"
                );
                Claim::Duplicate
            }
        }
    }

    /// Claims the results at `first_result_index..first_result_index + num_results` for the
    /// calling connection, as one batch.
    ///
    /// # Returns
    ///
    /// The number of leading results of the batch that have already been emitted. The caller must
    /// emit the remaining results, which may be none.
    ///
    /// # Panics
    ///
    /// Panics if `first_result_index` is above the next unclaimed result index, for the same
    /// reason as [`ArchiveCursor::claim`].
    pub fn claim_range(&self, first_result_index: u64, num_results: u64) -> u64 {
        let end = first_result_index + num_results;
        let mut current = self.next_result_index.load(Ordering::Relaxed);
        loop {
            assert!(
                current >= first_result_index,
                "result index {first_result_index} is above the next unclaimed result index \
                 {current}"
            );
            if current >= end {
                return num_results;
            }
            match self.next_result_index.compare_exchange_weak(
                current,
                end,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return current - first_result_index,
                Err(observed) => current = observed,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use clp_rust_utils::types::ArchiveId;

    use super::ArchiveCursor;
    use super::Claim;

    const ARCHIVE_ID: &str = "018e90e5-8b2a-4a61-a2fc-cac799936caf";

    /// # Returns
    ///
    /// A fresh cursor for the archive [`ARCHIVE_ID`].
    fn new_cursor() -> ArchiveCursor {
        ArchiveCursor::new(ARCHIVE_ID.parse::<ArchiveId>().expect("valid archive UUID"))
    }

    #[test]
    fn claims_indices_in_order() {
        let cursor = new_cursor();
        for result_index in 0..5 {
            assert_eq!(cursor.claim(result_index), Claim::Claimed);
        }
    }

    #[test]
    fn already_claimed_indices_are_duplicates() {
        let cursor = new_cursor();
        for result_index in 0..3 {
            assert_eq!(cursor.claim(result_index), Claim::Claimed);
        }

        for result_index in 0..3 {
            assert_eq!(cursor.claim(result_index), Claim::Duplicate);
        }
        assert_eq!(
            cursor.claim(3),
            Claim::Claimed,
            "duplicates shouldn't move the cursor"
        );
    }

    #[test]
    fn archive_id_is_the_one_the_cursor_was_created_with() {
        assert_eq!(
            new_cursor().archive_id(),
            ARCHIVE_ID.parse::<ArchiveId>().expect("valid archive UUID")
        );
    }

    #[test]
    fn concurrent_attempts_claim_each_index_exactly_once() {
        const NUM_ATTEMPTS: usize = 8;
        const NUM_RESULTS: u64 = 10_000;

        let cursor = Arc::new(new_cursor());
        let mut attempts = Vec::with_capacity(NUM_ATTEMPTS);
        for _ in 0..NUM_ATTEMPTS {
            let cursor = Arc::clone(&cursor);
            attempts.push(std::thread::spawn(move || {
                (0..NUM_RESULTS)
                    .filter(|&result_index| Claim::Claimed == cursor.claim(result_index))
                    .collect::<Vec<_>>()
            }));
        }

        let mut claimed = Vec::new();
        for attempt in attempts {
            claimed.extend(attempt.join().expect("attempt thread shouldn't panic"));
        }
        claimed.sort_unstable();
        assert_eq!(claimed, (0..NUM_RESULTS).collect::<Vec<_>>());
    }

    #[test]
    fn range_claims_emit_only_the_unclaimed_suffix() {
        let cursor = new_cursor();
        assert_eq!(cursor.claim_range(0, 4), 0);
        assert_eq!(
            cursor.claim_range(0, 2),
            2,
            "a fully emitted batch is a duplicate"
        );
        assert_eq!(cursor.claim_range(2, 5), 2, "the suffix 4..7 is claimed");
        assert_eq!(cursor.claim(6), Claim::Duplicate);
        assert_eq!(cursor.claim(7), Claim::Claimed);
        assert_eq!(cursor.claim_range(8, 0), 0);
        assert_eq!(
            cursor.claim(8),
            Claim::Claimed,
            "an empty batch claims nothing"
        );
    }

    #[test]
    #[should_panic(expected = "is above the next unclaimed result index")]
    fn range_claim_beyond_the_next_unclaimed_index_panics() {
        let cursor = new_cursor();
        cursor.claim_range(1, 3);
    }

    #[test]
    fn concurrent_range_and_single_claims_claim_each_index_exactly_once() {
        const NUM_ATTEMPTS: u64 = 8;
        const NUM_RESULTS: u64 = 100_000;

        let cursor = Arc::new(new_cursor());
        let mut attempts = Vec::new();
        for attempt in 0..NUM_ATTEMPTS {
            let cursor = Arc::clone(&cursor);
            attempts.push(std::thread::spawn(move || {
                // Attempts batch their results differently; one claims them one by one.
                let batch_size = if 0 == attempt { 1 } else { 7 + attempt * 13 };
                let mut claimed = Vec::new();
                let mut first = 0;
                while first < NUM_RESULTS {
                    let num_results = batch_size.min(NUM_RESULTS - first);
                    if 1 == batch_size {
                        if Claim::Claimed == cursor.claim(first) {
                            claimed.push(first);
                        }
                    } else {
                        let num_duplicates = cursor.claim_range(first, num_results);
                        claimed.extend(first + num_duplicates..first + num_results);
                    }
                    first += num_results;
                }
                claimed
            }));
        }

        let mut claimed = Vec::new();
        for attempt in attempts {
            claimed.extend(attempt.join().expect("attempt thread shouldn't panic"));
        }
        claimed.sort_unstable();
        assert_eq!(claimed, (0..NUM_RESULTS).collect::<Vec<_>>());
    }
}
