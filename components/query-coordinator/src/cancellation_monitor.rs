//! The monitor that processes job cancellation requests made through the API server.

use std::time::Duration;

use clp_rust_utils::job_config::QUERY_JOBS_TABLE_NAME;
use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::job_config::QueryJobStatus;
use const_format::formatcp;
use spider_client::error::ClientError;
use spider_core::types::id::JobId as SpiderJobId;
use sqlx::MySqlPool;
use tokio_util::sync::CancellationToken;

use crate::Error;
use crate::spider_job_submitter::SpiderJobSubmitter;

/// Processes the cancellation requests of jobs in [`QueryJobStatus::CancelRequested`].
///
/// # Type Parameters
///
/// * `SubmitterType` - The type of the job submitter used to cancel Spider jobs.
pub struct CancellationMonitor<SubmitterType: SpiderJobSubmitter> {
    db_pool: MySqlPool,
    job_submitter: SubmitterType,
    polling_interval: Duration,
}

impl<SubmitterType: SpiderJobSubmitter> CancellationMonitor<SubmitterType> {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// A newly created [`CancellationMonitor`].
    pub const fn new(
        db_pool: MySqlPool,
        job_submitter: SubmitterType,
        polling_interval: Duration,
    ) -> Self {
        Self {
            db_pool,
            job_submitter,
            polling_interval,
        }
    }

    /// Runs the monitor until `cancellation_token` is triggered.
    pub async fn run(self, cancellation_token: CancellationToken) {
        use tokio::select;

        loop {
            select! {
                () = cancellation_token.cancelled() => break,
                result = self.process_next_cancellation_request() => match result {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(e) => {
                        tracing::error!(
                            error = % e,
                            "Failed to process a job cancellation request."
                        );
                    }
                },
            }

            if tokio::time::timeout(self.polling_interval, cancellation_token.cancelled())
                .await
                .is_ok()
            {
                break;
            }
        }

        tracing::info!("Cancellation monitor shutting down.");
    }

    /// Terminates every job in [`QueryJobStatus::CancelRequested`] that has no recorded Spider job
    /// as [`QueryJobStatus::Cancelled`].
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`sqlx::query::QueryScalar::fetch_all`]'s return values on failure.
    pub async fn sweep_unsubmitted_jobs(&self) -> Result<(), Error> {
        const QUERY: &str = formatcp!(
            "SELECT `id` FROM `{QUERY_JOBS_TABLE_NAME}` WHERE `status` = ? AND `spider_id` IS \
             NULL ORDER BY `id` ASC"
        );

        let job_ids = sqlx::query_scalar::<_, QueryJobId>(QUERY)
            .bind(QueryJobStatus::CancelRequested)
            .fetch_all(&self.db_pool)
            .await?;

        for job_id in job_ids {
            tracing::info!(
                job_id = % job_id,
                "Cancelling a job that was never submitted.",
            );
            if let Err(e) = self.cancel_unsubmitted_job(job_id).await {
                tracing::error!(
                    error = % e,
                    job_id = % job_id,
                    "Failed to cancel a job that was never submitted. Skipping.",
                );
            }
        }

        Ok(())
    }

    /// Processes the cancellation request of the oldest job in [`QueryJobStatus::CancelRequested`],
    /// if any.
    ///
    /// # Returns
    ///
    /// Whether a cancellation request was processed on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::fetch_next_cancel_requested_job`]'s return values on failure.
    /// * Forwards [`Self::cancel`]'s return values on failure.
    async fn process_next_cancellation_request(&self) -> Result<bool, Error> {
        let Some(job) = self.fetch_next_cancel_requested_job().await? else {
            return Ok(false);
        };
        self.cancel(job).await?;
        Ok(true)
    }

    /// Fetches the oldest job in [`QueryJobStatus::CancelRequested`].
    ///
    /// # Returns
    ///
    /// The oldest job whose cancellation was requested, or `None` if there is none, on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`sqlx::query::QueryAs::fetch_optional`]'s return values on failure.
    async fn fetch_next_cancel_requested_job(
        &self,
    ) -> Result<Option<CancelRequestedJobRowProjection>, Error> {
        const QUERY: &str = formatcp!(
            "SELECT `id`, `spider_id` FROM `{QUERY_JOBS_TABLE_NAME}` WHERE `status` = ? ORDER BY \
             `id` ASC LIMIT 1"
        );

        Ok(sqlx::query_as::<_, CancelRequestedJobRowProjection>(QUERY)
            .bind(QueryJobStatus::CancelRequested)
            .fetch_optional(&self.db_pool)
            .await?)
    }

    /// Processes the cancellation request of `job`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::cancel_unsubmitted_job`]'s return values on failure.
    /// * Forwards [`Self::relay_cancellation_to_spider`]'s return values on failure.
    async fn cancel(&self, job: CancelRequestedJobRowProjection) -> Result<(), Error> {
        let Some(spider_job_id) = job.spider_job_id else {
            return self.cancel_unsubmitted_job(job.id).await;
        };
        self.relay_cancellation_to_spider(job.id, spider_job_id)
            .await
    }

    /// Relays the cancellation of the job identified by `job_id` to its Spider job identified by
    /// `spider_job_id`, and transitions the job to [`QueryJobStatus::Cancelling`].
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`SpiderJobSubmitter::cancel_job`]'s return values on failure, except
    ///   [`Error::SpiderClient`] with [`ClientError::JobNotFound`], which indicates the Spider job
    ///   doesn't exist and fails the job instead.
    /// * Forwards [`Self::transition_from_cancel_requested`]'s return values on failure.
    async fn relay_cancellation_to_spider(
        &self,
        job_id: QueryJobId,
        spider_job_id: SpiderJobId,
    ) -> Result<(), Error> {
        const SPIDER_JOB_NOT_FOUND_MESSAGE: &str = "The job's Spider job was not found.";

        match self.job_submitter.cancel_job(spider_job_id).await {
            Ok(()) => {}
            Err(Error::SpiderClient(ClientError::JobNotFound)) => {
                tracing::error!(
                    job_id = % job_id,
                    spider_job_id = % spider_job_id,
                    "The job's Spider job was not found. Failing the job.",
                );
                return self
                    .transition_from_cancel_requested(
                        job_id,
                        QueryJobStatus::Failed,
                        Some(SPIDER_JOB_NOT_FOUND_MESSAGE),
                    )
                    .await;
            }
            Err(e) => return Err(e),
        }
        tracing::info!(
            job_id = % job_id,
            spider_job_id = % spider_job_id,
            "Relayed the job cancellation to Spider.",
        );

        self.transition_from_cancel_requested(job_id, QueryJobStatus::Cancelling, None)
            .await
    }

    /// Terminates the job identified by `job_id`, which has no recorded Spider job, as
    /// [`QueryJobStatus::Cancelled`].
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`Self::transition_from_cancel_requested`]'s return values on failure.
    async fn cancel_unsubmitted_job(&self, job_id: QueryJobId) -> Result<(), Error> {
        const CANCELLED_MESSAGE: &str = "The job was cancelled before it was submitted.";

        self.transition_from_cancel_requested(
            job_id,
            QueryJobStatus::Cancelled,
            Some(CANCELLED_MESSAGE),
        )
        .await
    }

    /// Transitions the job identified by `job_id` from [`QueryJobStatus::CancelRequested`] to `to`.
    ///
    /// If the job is no longer in [`QueryJobStatus::CancelRequested`], nothing is persisted.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`Error::JobMetadataCorrupted`] if the job's row no longer exists.
    /// * Forwards [`sqlx::Pool::begin`]'s return values on failure.
    /// * Forwards [`sqlx::query::QueryScalar::fetch_optional`]'s return values on failure.
    /// * Forwards [`sqlx::query::Query::execute`]'s return values on failure.
    /// * Forwards [`sqlx::Transaction::commit`]'s return values on failure.
    async fn transition_from_cancel_requested(
        &self,
        job_id: QueryJobId,
        to: QueryJobStatus,
        status_msg: Option<&str>,
    ) -> Result<(), Error> {
        const SELECT_QUERY: &str =
            formatcp!("SELECT `status` FROM `{QUERY_JOBS_TABLE_NAME}` WHERE `id` = ? FOR UPDATE");
        const UPDATE_QUERY: &str = formatcp!(
            "UPDATE `{QUERY_JOBS_TABLE_NAME}` SET `status` = ?, `status_msg` = ? WHERE `id` = ?"
        );

        let mut tx = self.db_pool.begin().await?;
        let observed_status = sqlx::query_scalar::<_, QueryJobStatus>(SELECT_QUERY)
            .bind(job_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::JobMetadataCorrupted(job_id))?;
        if QueryJobStatus::CancelRequested != observed_status {
            tracing::info!(
                job_id = % job_id,
                observed_status = ? observed_status,
                to = ? to,
                "Skipping the cancellation transition: the job is no longer in \
                 `CancelRequested`.",
            );
            return Ok(());
        }

        sqlx::query(UPDATE_QUERY)
            .bind(to)
            .bind(status_msg)
            .bind(job_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        Ok(())
    }
}

/// A projection of the columns read from a [`QueryJobStatus::CancelRequested`] job row.
#[derive(Debug, sqlx::FromRow)]
struct CancelRequestedJobRowProjection {
    id: QueryJobId,
    #[sqlx(rename = "spider_id")]
    spider_job_id: Option<SpiderJobId>,
}
