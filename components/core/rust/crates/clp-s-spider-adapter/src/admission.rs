//! `WaitForBase` admission at the task boundary.
//!
//! The reader's memory budget is try-only: a stream whose base reservation does not fit fails the
//! search with `ResourceExhausted`. A Spider task runs one search per thread, so instead of failing
//! it waits until the budget can admit the reservation that failed, then runs the search again.

use std::time::Duration;
use std::time::Instant;

use clp_s_reader::MemoryBudget;
use clp_s_reader::ReadError;
use clp_s_reader::ResourceError;

/// A base reservation the memory budget could not admit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BudgetShortfall {
    pub requested_size: u64,
    pub limit_size: u64,
}

impl BudgetShortfall {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The shortfall `error` reports, or `None` if `error` is not a memory-budget exhaustion.
    pub fn from_error(error: &ReadError) -> Option<Self> {
        match error {
            ReadError::InArchive { source, .. } => Self::from_error(source),
            ReadError::Resource(resource_error) => match **resource_error {
                ResourceError::BudgetExhausted {
                    requested_size,
                    limit_size,
                } => Some(Self {
                    requested_size,
                    limit_size,
                }),
                _ => None,
            },
            _ => None,
        }
    }

    /// # Returns
    ///
    /// Whether the budget can ever admit the reservation, i.e., whether it fits in an empty budget.
    pub const fn is_admissible(&self) -> bool {
        self.requested_size <= self.limit_size
    }

    /// Waits until `budget` has room for the reservation or `deadline` passes.
    ///
    /// # Returns
    ///
    /// Whether the reservation fits in `budget` before `deadline`.
    pub fn wait_for_room(&self, budget: &MemoryBudget, deadline: Instant) -> bool {
        // The budget has no release notification, so the reserved size is polled; a task waits at
        // most once per failed attempt, and attempts are rare, so the polling cost is negligible.
        const POLL_INTERVAL: Duration = Duration::from_millis(10);

        loop {
            if budget
                .reserved_size()
                .checked_add(self.requested_size)
                .is_some_and(|size| size <= self.limit_size)
            {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            std::thread::sleep(POLL_INTERVAL.min(deadline - now));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use std::time::Instant;

    use clp_s_reader::ArchiveId;
    use clp_s_reader::MemoryBudget;
    use clp_s_reader::ReadError;
    use clp_s_reader::ResourceError;

    use super::BudgetShortfall;

    #[test]
    fn shortfalls_are_found_inside_archive_errors() {
        let exhausted = || {
            ReadError::from(ResourceError::BudgetExhausted {
                requested_size: 30,
                limit_size: 100,
            })
        };
        let expected = Some(BudgetShortfall {
            requested_size: 30,
            limit_size: 100,
        });
        assert_eq!(BudgetShortfall::from_error(&exhausted()), expected);
        assert_eq!(
            BudgetShortfall::from_error(&exhausted().in_archive(ArchiveId::from("a"))),
            expected
        );
        for error in [
            ReadError::from(ResourceError::AllocationFailed { size: 30 }),
            ReadError::from(ResourceError::Cancelled).in_archive(ArchiveId::from("a")),
            ReadError::Internal("bug".into()),
        ] {
            assert_eq!(BudgetShortfall::from_error(&error), None, "{error:?}");
        }
    }

    #[test]
    fn oversized_reservations_are_never_admissible() {
        let shortfall = |requested_size| BudgetShortfall {
            requested_size,
            limit_size: 100,
        };
        assert!(shortfall(100).is_admissible());
        assert!(!shortfall(101).is_admissible());
    }

    #[test]
    fn waits_end_when_the_budget_drains() -> anyhow::Result<()> {
        let budget = MemoryBudget::new(Some(100));
        let held = budget.try_reserve(80)?;
        let shortfall = BudgetShortfall {
            requested_size: 50,
            limit_size: 100,
        };
        let begin = Instant::now();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(held);
        });
        assert!(shortfall.wait_for_room(&budget, begin + Duration::from_secs(10)));
        assert!(begin.elapsed() >= Duration::from_millis(100));
        release
            .join()
            .map_err(|_| anyhow::anyhow!("the release thread panicked"))?;
        Ok(())
    }

    #[test]
    fn waits_time_out_while_the_budget_stays_full() -> anyhow::Result<()> {
        let budget = MemoryBudget::new(Some(100));
        let _held = budget.try_reserve(80)?;
        let shortfall = BudgetShortfall {
            requested_size: 50,
            limit_size: 100,
        };
        let begin = Instant::now();
        assert!(!shortfall.wait_for_room(&budget, begin + Duration::from_millis(50)));
        assert!(begin.elapsed() >= Duration::from_millis(50));

        let fitting = BudgetShortfall {
            requested_size: 20,
            limit_size: 100,
        };
        assert!(fitting.wait_for_room(&budget, begin));
        Ok(())
    }
}
