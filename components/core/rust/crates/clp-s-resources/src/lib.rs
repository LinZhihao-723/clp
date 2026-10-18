//! Host-scoped resources shared by every layer of the clp-s reader: cooperative cancellation, the
//! memory budget, the pool of decode buffers, and the error categories that every layer's errors
//! map onto.

mod budget;
mod buffer;
mod cancellation;
mod error;

pub use budget::MemoryBudget;
pub use budget::MemoryReservation;
pub use buffer::BufferPool;
pub use buffer::BufferPoolConfig;
pub use buffer::PooledBuffer;
pub use buffer::StreamBuffer;
pub use cancellation::CancellationToken;
pub use error::ErrorKind;
pub use error::ResourceError;

/// Shared, cheaply sliceable bytes. A frozen [`StreamBuffer`] converts into this type without a
/// copy, and its pooled allocation returns to the pool when the last clone is dropped.
pub type SharedBytes = bytes::Bytes;
