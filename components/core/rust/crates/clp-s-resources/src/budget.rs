//! A host-scoped memory budget with RAII reservations.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use crate::ResourceError;

/// A host-scoped byte budget shared by every scan of a reader context.
///
/// Reservations are try-only: nothing ever blocks waiting for memory. Each reservation is released
/// when its [`MemoryReservation`] is dropped.
#[derive(Debug)]
pub struct MemoryBudget {
    limit_size: Option<u64>,
    reserved_size: AtomicU64,
}

impl MemoryBudget {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A new budget limited to `limit_size` bytes, or an unlimited budget if `limit_size` is
    /// `None` (CLI parity with C++).
    #[must_use]
    pub fn new(limit_size: Option<u64>) -> Arc<Self> {
        Arc::new(Self {
            limit_size,
            reserved_size: AtomicU64::new(0),
        })
    }

    /// Reserves `size` bytes if they fit in the remaining budget.
    ///
    /// # Returns
    ///
    /// The reservation on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ResourceError::BudgetExhausted`] if `size` does not fit in the remaining budget.
    pub fn try_reserve(self: &Arc<Self>, size: u64) -> Result<MemoryReservation, ResourceError> {
        let limit_size = self.limit_size.unwrap_or(u64::MAX);
        self.reserved_size
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |reserved_size| {
                reserved_size
                    .checked_add(size)
                    .filter(|&new_reserved_size| new_reserved_size <= limit_size)
            })
            .map_err(|_| ResourceError::BudgetExhausted {
                requested_size: size,
                limit_size,
            })?;
        Ok(MemoryReservation {
            budget: Arc::clone(self),
            size,
        })
    }

    /// # Returns
    ///
    /// The configured limit, or `None` if the budget is unlimited.
    #[must_use]
    pub const fn limit_size(&self) -> Option<u64> {
        self.limit_size
    }

    /// # Returns
    ///
    /// The number of bytes currently reserved.
    #[must_use]
    pub fn reserved_size(&self) -> u64 {
        self.reserved_size.load(Ordering::Acquire)
    }

    /// Returns `size` bytes to the budget.
    fn release(&self, size: u64) {
        let previous_reserved_size = self.reserved_size.fetch_sub(size, Ordering::AcqRel);
        debug_assert!(
            previous_reserved_size >= size,
            "released more bytes than were reserved"
        );
    }
}

/// An RAII share of a [`MemoryBudget`]; dropping it releases the bytes.
#[derive(Debug)]
pub struct MemoryReservation {
    budget: Arc<MemoryBudget>,
    size: u64,
}

impl MemoryReservation {
    /// # Returns
    ///
    /// The number of bytes this reservation holds.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }
}

impl Drop for MemoryReservation {
    fn drop(&mut self) {
        self.budget.release(self.size);
    }
}

#[cfg(test)]
mod tests {
    use super::MemoryBudget;
    use crate::ResourceError;

    #[test]
    fn reservations_are_admitted_up_to_the_limit_and_released_on_drop() -> anyhow::Result<()> {
        const LIMIT_SIZE: u64 = 100;
        let budget = MemoryBudget::new(Some(LIMIT_SIZE));
        assert_eq!(budget.limit_size(), Some(LIMIT_SIZE));

        let first = budget.try_reserve(60)?;
        let second = budget.try_reserve(LIMIT_SIZE - 60)?;
        assert_eq!((first.size(), second.size()), (60, 40));
        assert_eq!(budget.reserved_size(), LIMIT_SIZE);

        let result = budget.try_reserve(1);
        assert!(
            matches!(
                result,
                Err(ResourceError::BudgetExhausted {
                    requested_size: 1,
                    limit_size: LIMIT_SIZE,
                })
            ),
            "{result:?}"
        );
        assert_eq!(budget.reserved_size(), LIMIT_SIZE);

        drop(first);
        assert_eq!(budget.reserved_size(), 40);
        let third = budget.try_reserve(60)?;
        drop((second, third));
        assert_eq!(budget.reserved_size(), 0);
        Ok(())
    }

    #[test]
    fn unlimited_budgets_admit_everything_that_does_not_overflow() -> anyhow::Result<()> {
        let budget = MemoryBudget::new(None);
        assert_eq!(budget.limit_size(), None);
        let huge = budget.try_reserve(u64::MAX - 1)?;
        let result = budget.try_reserve(2);
        assert!(
            matches!(result, Err(ResourceError::BudgetExhausted { .. })),
            "{result:?}"
        );
        drop(huge);
        assert_eq!(budget.reserved_size(), 0);
        Ok(())
    }

    #[test]
    fn zero_sized_reservations_always_fit() -> anyhow::Result<()> {
        let budget = MemoryBudget::new(Some(0));
        let empty = budget.try_reserve(0)?;
        assert_eq!(empty.size(), 0);
        assert_eq!(budget.reserved_size(), 0);
        Ok(())
    }

    #[test]
    fn concurrent_reservations_never_exceed_the_limit() {
        const LIMIT_SIZE: u64 = 64;
        let budget = MemoryBudget::new(Some(LIMIT_SIZE));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..1_000 {
                        if let Ok(reservation) = budget.try_reserve(16) {
                            assert!(budget.reserved_size() <= LIMIT_SIZE);
                            drop(reservation);
                        }
                    }
                });
            }
        });
        assert_eq!(budget.reserved_size(), 0);
    }
}
