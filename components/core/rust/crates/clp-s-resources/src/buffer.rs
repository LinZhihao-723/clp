//! A pool of large decode buffers recycled across streams, scans and archives.

use std::sync::Arc;
use std::sync::Weak;

use parking_lot::Mutex;

use crate::MemoryReservation;
use crate::ResourceError;
use crate::SharedBytes;

/// Pool limits; buffers released above them are freed instead of retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufferPoolConfig {
    /// Total capacity, in bytes, of the buffers the pool keeps on its free list.
    pub retained_size_limit: u64,

    /// Maximum number of buffers the pool keeps on its free list.
    pub max_retained_buffers: usize,
}

impl Default for BufferPoolConfig {
    fn default() -> Self {
        Self {
            retained_size_limit: 1 << 30,
            max_retained_buffers: 4,
        }
    }
}

/// Recycles large decode buffers; buffers are never zero-filled.
///
/// # NOTE
///
/// The pool does not yet bound real memory by a [`crate::MemoryBudget`]: a recycled buffer can be
/// larger than the reservation it is paired with, and buffers on the free list are charged to no
/// budget. This is harmless while the budget is unlimited by default; a limited budget needs the
/// pool to reserve a recycled buffer's actual capacity or to charge retained buffers.
#[derive(Debug)]
pub struct BufferPool {
    config: BufferPoolConfig,
    free_list: Mutex<FreeList>,
}

impl BufferPool {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A new, empty pool.
    #[must_use]
    pub fn new(config: BufferPoolConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            free_list: Mutex::new(FreeList::default()),
        })
    }

    /// Acquires a buffer with at least `capacity` bytes of spare capacity, recycled if possible.
    ///
    /// The smallest retained buffer that is large enough is recycled; otherwise a new buffer is
    /// allocated. The buffer holds `reservation` until the buffer, or the [`StreamBuffer`] frozen
    /// from it, is dropped.
    ///
    /// # Returns
    ///
    /// An empty buffer with at least `capacity` bytes of spare capacity on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ResourceError::AllocationFailed`] if the allocation fails or `capacity` does not fit in
    ///   `usize`.
    pub fn acquire(
        self: &Arc<Self>,
        capacity: u64,
        reservation: MemoryReservation,
    ) -> Result<PooledBuffer, ResourceError> {
        let allocation_failed = || ResourceError::AllocationFailed { size: capacity };
        let capacity = usize::try_from(capacity).map_err(|_| allocation_failed())?;
        let recycled = self.free_list.lock().take(capacity);
        let bytes = if let Some(bytes) = recycled {
            bytes
        } else {
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(capacity)
                .map_err(|_| allocation_failed())?;
            bytes
        };
        Ok(PooledBuffer {
            bytes,
            pool: Arc::downgrade(self),
            reservation,
        })
    }

    /// # Returns
    ///
    /// The total capacity, in bytes, of the buffers on the free list.
    #[must_use]
    pub fn retained_size(&self) -> u64 {
        self.free_list.lock().retained_size
    }

    /// # Returns
    ///
    /// The number of buffers on the free list.
    #[must_use]
    pub fn num_retained_buffers(&self) -> usize {
        self.free_list.lock().buffers.len()
    }

    /// Takes back a released buffer, keeping it on the free list if the limits allow.
    fn release(&self, bytes: Vec<u8>) {
        self.free_list.lock().put(bytes, &self.config);
    }
}

/// An exclusively owned pool buffer that a decoder fills through its spare capacity.
#[derive(Debug)]
pub struct PooledBuffer {
    bytes: Vec<u8>,
    pool: Weak<BufferPool>,
    reservation: MemoryReservation,
}

impl PooledBuffer {
    /// Exposes the vector so a decoder can write into its spare capacity (`zstd-safe`'s
    /// `WriteBuf` is implemented for `Vec<u8>`).
    ///
    /// # Returns
    ///
    /// The underlying vector.
    pub const fn as_mut_vec(&mut self) -> &mut Vec<u8> {
        &mut self.bytes
    }

    /// # Returns
    ///
    /// The reservation this buffer holds.
    #[must_use]
    pub const fn reservation(&self) -> &MemoryReservation {
        &self.reservation
    }

    /// Makes the buffer immutable and shareable.
    ///
    /// # Returns
    ///
    /// The frozen buffer.
    #[must_use]
    pub fn freeze(self) -> StreamBuffer {
        StreamBuffer {
            buffer: Arc::new(self),
        }
    }
}

impl Drop for PooledBuffer {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.upgrade() {
            pool.release(std::mem::take(&mut self.bytes));
        }
    }
}

/// Immutable decoded bytes; the last clone returns the allocation and its reservation.
#[derive(Clone, Debug)]
pub struct StreamBuffer {
    buffer: Arc<PooledBuffer>,
}

impl StreamBuffer {
    /// # Returns
    ///
    /// The decoded bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buffer.bytes
    }

    /// Converts the buffer into [`SharedBytes`] without copying (through
    /// [`bytes::Bytes::from_owner`]).
    ///
    /// # Returns
    ///
    /// The shared bytes.
    #[must_use]
    pub fn into_shared_bytes(self) -> SharedBytes {
        SharedBytes::from_owner(self)
    }
}

impl AsRef<[u8]> for StreamBuffer {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

const _: () = {
    const fn assert_send_sync<SendSyncType: Send + Sync + 'static>() {}
    assert_send_sync::<BufferPool>();
    assert_send_sync::<PooledBuffer>();
    assert_send_sync::<StreamBuffer>();
};

/// The retained buffers of a pool, in no particular order.
#[derive(Debug, Default)]
struct FreeList {
    buffers: Vec<Vec<u8>>,
    retained_size: u64,
}

impl FreeList {
    /// Takes the smallest retained buffer whose capacity is at least `capacity`.
    ///
    /// # Returns
    ///
    /// The emptied buffer, or `None` if no retained buffer is large enough.
    fn take(&mut self, capacity: usize) -> Option<Vec<u8>> {
        let (index, _) = self
            .buffers
            .iter()
            .enumerate()
            .filter(|(_, buffer)| buffer.capacity() >= capacity)
            .min_by_key(|(_, buffer)| buffer.capacity())?;
        let mut buffer = self.buffers.swap_remove(index);
        self.retained_size -= capacity_size(&buffer);
        buffer.clear();
        Some(buffer)
    }

    /// Retains `buffer` if it fits in the limits of `config`, evicting smaller retained buffers to
    /// make room; otherwise frees it.
    fn put(&mut self, buffer: Vec<u8>, config: &BufferPoolConfig) {
        let size = capacity_size(&buffer);
        if 0 == size || size > config.retained_size_limit || 0 == config.max_retained_buffers {
            return;
        }
        while self.retained_size.saturating_add(size) > config.retained_size_limit
            || self.buffers.len() >= config.max_retained_buffers
        {
            let Some((index, smallest)) = self
                .buffers
                .iter()
                .enumerate()
                .min_by_key(|(_, retained)| retained.capacity())
            else {
                break;
            };
            if capacity_size(smallest) >= size {
                return;
            }
            let evicted = self.buffers.swap_remove(index);
            self.retained_size -= capacity_size(&evicted);
        }
        self.retained_size += size;
        self.buffers.push(buffer);
    }
}

/// # Returns
///
/// The capacity of `buffer` in bytes.
fn capacity_size(buffer: &Vec<u8>) -> u64 {
    u64::try_from(buffer.capacity()).expect("a `usize` capacity fits in `u64`")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::BufferPool;
    use super::BufferPoolConfig;
    use crate::MemoryBudget;
    use crate::MemoryReservation;
    use crate::ResourceError;

    const BASE_CONFIG: BufferPoolConfig = BufferPoolConfig {
        retained_size_limit: 1 << 20,
        max_retained_buffers: 4,
    };

    /// # Returns
    ///
    /// A reservation of `size` bytes from an unlimited budget.
    fn reservation(budget: &Arc<MemoryBudget>, size: u64) -> MemoryReservation {
        budget
            .try_reserve(size)
            .expect("an unlimited budget admits every reservation")
    }

    #[test]
    fn acquired_buffers_are_empty_with_spare_capacity() -> anyhow::Result<()> {
        let budget = MemoryBudget::new(None);
        let pool = BufferPool::new(BASE_CONFIG);
        let mut buffer = pool.acquire(1_000, reservation(&budget, 1_000))?;
        assert_eq!(*buffer.as_mut_vec(), Vec::<u8>::new());
        assert!(buffer.as_mut_vec().capacity() >= 1_000);
        assert_eq!(buffer.reservation().size(), 1_000);
        Ok(())
    }

    #[test]
    fn released_buffers_are_recycled_without_reallocation() -> anyhow::Result<()> {
        let budget = MemoryBudget::new(None);
        let pool = BufferPool::new(BASE_CONFIG);

        let mut buffer = pool.acquire(4_096, reservation(&budget, 4_096))?;
        buffer.as_mut_vec().extend_from_slice(b"decoded");
        let allocation = buffer.as_mut_vec().as_ptr();
        drop(buffer);
        assert_eq!(pool.num_retained_buffers(), 1);
        assert_eq!(budget.reserved_size(), 0);

        let mut recycled = pool.acquire(1_000, reservation(&budget, 1_000))?;
        assert_eq!(recycled.as_mut_vec().as_ptr(), allocation);
        assert_eq!(*recycled.as_mut_vec(), Vec::<u8>::new());
        assert_eq!(pool.num_retained_buffers(), 0);
        assert_eq!(pool.retained_size(), 0);
        Ok(())
    }

    #[test]
    fn the_smallest_sufficient_buffer_is_recycled() -> anyhow::Result<()> {
        let budget = MemoryBudget::new(None);
        let pool = BufferPool::new(BASE_CONFIG);
        let mut small = pool.acquire(100, reservation(&budget, 100))?;
        let mut medium = pool.acquire(1_000, reservation(&budget, 1_000))?;
        let mut large = pool.acquire(10_000, reservation(&budget, 10_000))?;
        let medium_allocation = medium.as_mut_vec().as_ptr();
        drop((small, large, medium));
        assert_eq!(pool.num_retained_buffers(), 3);

        small = pool.acquire(500, reservation(&budget, 500))?;
        assert_eq!(small.as_mut_vec().as_ptr(), medium_allocation);

        let before = pool.num_retained_buffers();
        large = pool.acquire(20_000, reservation(&budget, 20_000))?;
        assert!(large.as_mut_vec().capacity() >= 20_000);
        assert_eq!(pool.num_retained_buffers(), before);
        Ok(())
    }

    #[test]
    fn frozen_buffers_return_to_the_pool_when_the_last_clone_drops() -> anyhow::Result<()> {
        let budget = MemoryBudget::new(None);
        let pool = BufferPool::new(BASE_CONFIG);
        let mut buffer = pool.acquire(64, reservation(&budget, 64))?;
        buffer.as_mut_vec().extend_from_slice(b"stream bytes");

        let frozen = buffer.freeze();
        let clone = frozen.clone();
        assert_eq!(frozen.as_bytes(), b"stream bytes");
        assert_eq!(clone.as_ref(), b"stream bytes");

        let shared = frozen.into_shared_bytes();
        let slice = shared.slice(7..);
        drop(shared);
        assert_eq!(&slice[..], b"bytes");
        drop(clone);
        assert_eq!(pool.num_retained_buffers(), 0);
        assert_eq!(budget.reserved_size(), 64);

        drop(slice);
        assert_eq!(pool.num_retained_buffers(), 1);
        assert_eq!(budget.reserved_size(), 0);
        Ok(())
    }

    #[test]
    fn retention_respects_the_size_and_count_limits() -> anyhow::Result<()> {
        let budget = MemoryBudget::new(None);
        let config = BufferPoolConfig {
            retained_size_limit: 10_000,
            max_retained_buffers: 2,
        };
        let pool = BufferPool::new(config);

        drop(pool.acquire(20_000, reservation(&budget, 20_000))?);
        assert_eq!(pool.num_retained_buffers(), 0);

        let buffers = [100, 200, 300].map(|size| {
            pool.acquire(size, reservation(&budget, size))
                .expect("small allocations succeed")
        });
        drop(buffers);
        assert_eq!(pool.num_retained_buffers(), config.max_retained_buffers);
        assert!(pool.retained_size() >= 500, "{}", pool.retained_size());

        let mut large = pool.acquire(9_000, reservation(&budget, 9_000))?;
        let large_allocation = large.as_mut_vec().as_ptr();
        drop(large);
        assert_eq!(pool.num_retained_buffers(), config.max_retained_buffers);
        assert!(pool.retained_size() <= config.retained_size_limit);
        let mut recycled = pool.acquire(9_000, reservation(&budget, 9_000))?;
        assert_eq!(recycled.as_mut_vec().as_ptr(), large_allocation);
        Ok(())
    }

    #[test]
    fn a_pool_without_retention_frees_every_buffer() -> anyhow::Result<()> {
        let budget = MemoryBudget::new(None);
        let pool = BufferPool::new(BufferPoolConfig {
            max_retained_buffers: 0,
            ..BASE_CONFIG
        });
        drop(pool.acquire(64, reservation(&budget, 64))?);
        assert_eq!(pool.num_retained_buffers(), 0);
        Ok(())
    }

    #[test]
    fn buffers_outliving_their_pool_are_freed() -> anyhow::Result<()> {
        let budget = MemoryBudget::new(None);
        let pool = BufferPool::new(BASE_CONFIG);
        let buffer = pool.acquire(64, reservation(&budget, 64))?.freeze();
        drop(pool);
        drop(buffer);
        assert_eq!(budget.reserved_size(), 0);
        Ok(())
    }

    #[test]
    fn impossible_allocations_fail_without_panicking() {
        let budget = MemoryBudget::new(None);
        let pool = BufferPool::new(BASE_CONFIG);
        for capacity in [u64::MAX, 1 << 62] {
            let result = pool.acquire(capacity, reservation(&budget, 0));
            assert!(
                matches!(result, Err(ResourceError::AllocationFailed { size }) if size == capacity),
                "capacity {capacity}: {result:?}"
            );
        }
        assert_eq!(budget.reserved_size(), 0);
    }
}
