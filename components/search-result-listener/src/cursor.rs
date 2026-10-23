//! The per-task cursor that lets exactly one attempt of a task emit each of its results.

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use clp_rust_utils::types::ArchiveId;

/// The outcome of claiming a result index on a [`TaskCursor`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Claim {
    /// The caller is the first to deliver the result, so it must emit it.
    Claimed,

    /// An attempt of the task already emitted the result, so the caller must drop it.
    Duplicate,
}

/// Tracks which results of one query task have been emitted across every attempt of the task.
#[derive(Debug)]
pub struct TaskCursor {
    next_result_index: AtomicU64,
    archive_id: ArchiveId,
}

impl TaskCursor {
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
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use clp_rust_utils::types::ArchiveId;

    use super::Claim;
    use super::TaskCursor;

    const ARCHIVE_ID: &str = "018e90e5-8b2a-4a61-a2fc-cac799936caf";

    /// # Returns
    ///
    /// A fresh cursor for the archive [`ARCHIVE_ID`].
    fn new_cursor() -> TaskCursor {
        TaskCursor::new(ARCHIVE_ID.parse::<ArchiveId>().expect("valid archive UUID"))
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
}
