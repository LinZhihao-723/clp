//! Reader contexts and execution configuration.

use std::num::NonZeroU32;
use std::sync::Arc;

use clp_s_format::OpenOptions;
use clp_s_resources::BufferPool;
use clp_s_resources::BufferPoolConfig;
use clp_s_resources::MemoryBudget;
use clp_s_table::DEFAULT_BATCH_NUM_ROWS;

use crate::ReadError;

/// Where scan work runs; only [`Executor::CurrentThread`] exists in this round.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Executor {
    /// Everything runs on the thread that pulls the scan or runs the session; nothing is spawned.
    #[default]
    CurrentThread,
}

/// Tunables with measured defaults.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionConfig {
    /// Rows per scan window (default 4,096).
    pub batch_num_rows: NonZeroU32,

    /// Bytes of rendered JSON after which a window is delivered as several batches (default
    /// 256 KiB).
    pub max_rendered_batch_size: u64,

    /// Compressed chunk size for streaming decode of local files (default 1 MiB).
    pub stream_chunk_size: u64,

    pub open_options: OpenOptions,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            batch_num_rows: NonZeroU32::new(DEFAULT_BATCH_NUM_ROWS).unwrap_or(NonZeroU32::MIN),
            max_rendered_batch_size: 256 * 1024,
            stream_chunk_size: 1024 * 1024,
            open_options: OpenOptions::default(),
        }
    }
}

/// Host-scoped resources shared by every scan given this context.
#[derive(Debug)]
pub struct ReaderContext {
    executor: Executor,
    config: ExecutionConfig,
    buffer_pool: Arc<BufferPool>,
    memory_budget: Arc<MemoryBudget>,
}

impl ReaderContext {
    /// # Returns
    ///
    /// A builder with the default configuration and an unlimited memory budget.
    #[must_use]
    pub fn builder() -> ReaderContextBuilder {
        ReaderContextBuilder {
            config: ExecutionConfig::default(),
            memory_budget_size: None,
        }
    }

    /// # Returns
    ///
    /// The executor.
    #[must_use]
    pub const fn executor(&self) -> Executor {
        self.executor
    }

    /// # Returns
    ///
    /// The execution configuration.
    #[must_use]
    pub const fn config(&self) -> &ExecutionConfig {
        &self.config
    }

    /// # Returns
    ///
    /// The decode-buffer pool.
    #[must_use]
    pub const fn buffer_pool(&self) -> &Arc<BufferPool> {
        &self.buffer_pool
    }

    /// # Returns
    ///
    /// The memory budget.
    #[must_use]
    pub const fn memory_budget(&self) -> &Arc<MemoryBudget> {
        &self.memory_budget
    }
}

/// Builds a [`ReaderContext`].
#[derive(Debug)]
pub struct ReaderContextBuilder {
    config: ExecutionConfig,
    memory_budget_size: Option<u64>,
}

impl ReaderContextBuilder {
    /// # Returns
    ///
    /// The builder with the memory budget limited to `limit_size` bytes (`None` is unlimited).
    #[must_use]
    pub const fn memory_budget_size(self, limit_size: Option<u64>) -> Self {
        Self {
            memory_budget_size: limit_size,
            ..self
        }
    }

    /// # Returns
    ///
    /// The builder with `config` as the execution configuration.
    #[must_use]
    pub const fn config(self, config: ExecutionConfig) -> Self {
        Self { config, ..self }
    }

    /// Builds the context.
    ///
    /// # Returns
    ///
    /// The context on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`ReadError::InvalidArgument`] if the configuration is inconsistent (for example a zero
    ///   chunk size).
    pub fn build(self) -> Result<Arc<ReaderContext>, ReadError> {
        if 0 == self.config.stream_chunk_size {
            return Err(ReadError::InvalidArgument(
                "the stream chunk size must be positive".into(),
            ));
        }
        if 0 == self.config.max_rendered_batch_size {
            return Err(ReadError::InvalidArgument(
                "the maximum rendered batch size must be positive".into(),
            ));
        }
        Ok(Arc::new(ReaderContext {
            executor: Executor::CurrentThread,
            config: self.config,
            buffer_pool: BufferPool::new(BufferPoolConfig::default()),
            memory_budget: MemoryBudget::new(self.memory_budget_size),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::ExecutionConfig;
    use super::Executor;
    use super::ReaderContext;
    use crate::ReadError;

    #[test]
    fn defaults_match_the_design() -> anyhow::Result<()> {
        let context = ReaderContext::builder().build()?;
        assert_eq!(context.executor(), Executor::CurrentThread);
        assert_eq!(context.config().batch_num_rows.get(), 4096);
        assert_eq!(context.config().max_rendered_batch_size, 256 * 1024);
        assert_eq!(context.config().stream_chunk_size, 1024 * 1024);
        assert_eq!(context.memory_budget().limit_size(), None);
        Ok(())
    }

    #[test]
    fn the_budget_limit_is_applied() -> anyhow::Result<()> {
        let context = ReaderContext::builder()
            .memory_budget_size(Some(1000))
            .build()?;
        assert_eq!(context.memory_budget().limit_size(), Some(1000));
        Ok(())
    }

    #[test]
    fn zero_sizes_are_rejected() {
        let base = ExecutionConfig::default();
        for config in [
            ExecutionConfig {
                stream_chunk_size: 0,
                ..base
            },
            ExecutionConfig {
                max_rendered_batch_size: 0,
                ..base
            },
        ] {
            let result = ReaderContext::builder().config(config).build();
            assert!(
                matches!(result, Err(ReadError::InvalidArgument(_))),
                "{config:?}: {result:?}"
            );
        }
    }
}
