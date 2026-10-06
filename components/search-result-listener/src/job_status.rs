//! Sources of query job statuses, which a session polls to learn when its job has terminated.

use std::time::Duration;

use async_trait::async_trait;
use clp_rust_utils::job_config::QUERY_JOBS_TABLE_NAME;
use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::job_config::QueryJobStatus;
use const_format::formatcp;

use crate::Error;

/// A source of query job statuses.
#[async_trait]
pub trait JobStatusSource: Send + Sync {
    /// # Returns
    ///
    /// The delay between two consecutive status polls.
    fn poll_interval(&self) -> Duration;

    /// Fetches a query job's current status.
    ///
    /// # Parameters
    ///
    /// * `query_job_id` - The ID of the query job.
    ///
    /// # Returns
    ///
    /// The query job's current status on success.
    ///
    /// # Errors
    ///
    /// Implementations must document their error conditions.
    async fn get_status(&self, query_job_id: QueryJobId) -> Result<QueryJobStatus, Error>;
}

/// A [`JobStatusSource`] that reads statuses from the query jobs table of the CLP database.
pub struct MariaDbJobStatusSource {
    db_pool: sqlx::MySqlPool,
    poll_interval: Duration,
}

impl MariaDbJobStatusSource {
    #[must_use]
    pub const fn new(db_pool: sqlx::MySqlPool, poll_interval: Duration) -> Self {
        Self {
            db_pool,
            poll_interval,
        }
    }
}

#[async_trait]
impl JobStatusSource for MariaDbJobStatusSource {
    fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Error::QueryJobNotFound`] if the query job's row doesn't exist.
    /// * Forwards [`sqlx::query::QueryScalar::fetch_optional`]'s return values on failure.
    async fn get_status(&self, query_job_id: QueryJobId) -> Result<QueryJobStatus, Error> {
        const QUERY: &str =
            formatcp!("SELECT `status` FROM `{QUERY_JOBS_TABLE_NAME}` WHERE `id` = ?");

        sqlx::query_scalar::<_, QueryJobStatus>(QUERY)
            .bind(query_job_id)
            .fetch_optional(&self.db_pool)
            .await?
            .ok_or(Error::QueryJobNotFound(query_job_id))
    }
}
