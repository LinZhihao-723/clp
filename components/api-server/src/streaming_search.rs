//! Streaming searches, each of which submits a search job whose search tasks stream their results
//! to this server, and streams the results back to the caller as they arrive.

use std::future::Future;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::task::ready;
use std::time::Duration;

use async_trait::async_trait;
use clp_rust_utils::database::mysql::cancel_query_job;
use clp_rust_utils::database::mysql::submit_query_job;
use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::job_config::SearchJobConfig;
use futures::Stream;
use search_result_listener::JobStatusSource;
use search_result_listener::MariaDbJobStatusSource;
use search_result_listener::OutcomeFuture;
use search_result_listener::ResultListener;
use search_result_listener::ResultStream;
use search_result_listener::SearchResult;
use search_result_listener::SessionConfig;
use search_result_listener::SessionOutcome;

use crate::client::ClientError;
use crate::client::QueryConfig;

/// The operations a streaming search performs on the query jobs table.
#[async_trait]
pub trait QueryJobTable: Clone + Send + Sync + Unpin + 'static {
    /// The source of the query jobs' statuses that a streaming search polls.
    type StatusSource: JobStatusSource + 'static;

    /// Submits a search job.
    ///
    /// # Parameters
    ///
    /// * `search_job_config` - The config of the search job.
    ///
    /// # Returns
    ///
    /// The ID of the submitted query job on success.
    ///
    /// # Errors
    ///
    /// Implementations must document their error conditions.
    async fn submit(&self, search_job_config: &SearchJobConfig) -> Result<QueryJobId, ClientError>;

    /// Requests the cancellation of a query job by marking it as cancelling, if it is pending or
    /// running.
    ///
    /// # Parameters
    ///
    /// * `query_job_id` - The ID of the query job.
    ///
    /// # Returns
    ///
    /// Whether the query job was marked on success.
    ///
    /// # Errors
    ///
    /// Implementations must document their error conditions.
    async fn cancel(&self, query_job_id: QueryJobId) -> Result<bool, ClientError>;

    /// # Returns
    ///
    /// A source of the query jobs' statuses.
    fn status_source(&self) -> Self::StatusSource;
}

/// A [`QueryJobTable`] that operates on the query jobs table of the CLP database.
#[derive(Clone)]
pub struct MariaDbQueryJobTable {
    db_pool: sqlx::MySqlPool,
}

impl MariaDbQueryJobTable {
    #[must_use]
    pub const fn new(db_pool: sqlx::MySqlPool) -> Self {
        Self { db_pool }
    }
}

#[async_trait]
impl QueryJobTable for MariaDbQueryJobTable {
    type StatusSource = MariaDbJobStatusSource;

    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`submit_query_job`]'s return values on failure.
    async fn submit(&self, search_job_config: &SearchJobConfig) -> Result<QueryJobId, ClientError> {
        Ok(submit_query_job(&self.db_pool, search_job_config).await?)
    }

    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`cancel_query_job`]'s return values on failure.
    async fn cancel(&self, query_job_id: QueryJobId) -> Result<bool, ClientError> {
        Ok(cancel_query_job(&self.db_pool, query_job_id).await?)
    }

    fn status_source(&self) -> MariaDbJobStatusSource {
        MariaDbJobStatusSource::new(self.db_pool.clone(), JOB_STATUS_POLL_INTERVAL)
    }
}

/// Runs streaming searches, receiving the results of every search through one listener.
///
/// # Type Parameters
///
/// * `QueryJobTableType` - The query jobs table that searches are submitted to.
pub struct StreamingSearch<QueryJobTableType: QueryJobTable> {
    listener: ResultListener,
    query_job_table: QueryJobTableType,
}

impl<QueryJobTableType: QueryJobTable> StreamingSearch<QueryJobTableType> {
    #[must_use]
    pub const fn new(listener: ResultListener, query_job_table: QueryJobTableType) -> Self {
        Self {
            listener,
            query_job_table,
        }
    }

    /// Submits a search job whose search tasks stream their results to this server.
    ///
    /// # Returns
    ///
    /// The stream of the search's events on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`QueryConfig::into_streaming_search_job_config`]'s return values on failure.
    /// * Forwards [`QueryJobTable::submit`]'s return values on failure.
    pub async fn submit(
        &self,
        query_config: QueryConfig,
    ) -> Result<SearchStream<QueryJobTableType>, ClientError> {
        let mut search_job_config = query_config.into_streaming_search_job_config()?;
        let session = self.listener.open_session(SessionConfig::default());
        search_job_config.network_output = Some(session.network_output().clone());
        let query_job_id = self.query_job_table.submit(&search_job_config).await?;
        tracing::debug!(
            query_job_id,
            session_token = % session.token(),
            "Inserted the streaming search's query job."
        );

        let (results, outcome) = session.run(query_job_id, self.query_job_table.status_source());
        Ok(SearchStream {
            query_job_id,
            results,
            have_results_ended: false,
            outcome: Some(outcome),
            query_job_table: self.query_job_table.clone(),
        })
    }
}

/// An event of a streaming search.
#[derive(Debug)]
pub enum SearchEvent {
    /// A search result.
    Result(SearchResult),

    /// The end of the search, once its query job has terminated and every result has been
    /// streamed, with the search's outcome.
    End(Result<SessionOutcome, search_result_listener::Error>),
}

/// The events of a streaming search: each of its results as it arrives, then its end.
///
/// Dropping the stream before its results have ended requests the cancellation of its query job.
///
/// # Type Parameters
///
/// * `QueryJobTableType` - The query jobs table that the search was submitted to.
pub struct SearchStream<QueryJobTableType: QueryJobTable> {
    query_job_id: QueryJobId,
    results: ResultStream,
    have_results_ended: bool,

    /// `None` once the end has been streamed.
    outcome: Option<OutcomeFuture>,

    query_job_table: QueryJobTableType,
}

impl<QueryJobTableType: QueryJobTable> SearchStream<QueryJobTableType> {
    #[must_use]
    pub const fn query_job_id(&self) -> QueryJobId {
        self.query_job_id
    }
}

impl<QueryJobTableType: QueryJobTable> Stream for SearchStream<QueryJobTableType> {
    type Item = SearchEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if !this.have_results_ended {
            match ready!(Pin::new(&mut this.results).poll_next(cx)) {
                Some(result) => return Poll::Ready(Some(SearchEvent::Result(result))),
                None => this.have_results_ended = true,
            }
        }
        let Some(outcome) = this.outcome.as_mut() else {
            return Poll::Ready(None);
        };
        let outcome = ready!(Pin::new(outcome).poll(cx));
        this.outcome = None;
        Poll::Ready(Some(SearchEvent::End(outcome)))
    }
}

impl<QueryJobTableType: QueryJobTable> Drop for SearchStream<QueryJobTableType> {
    fn drop(&mut self) {
        if self.have_results_ended {
            return;
        }
        let outcome = self
            .outcome
            .take()
            .expect("the outcome should be kept until the results have ended");
        let query_job_id = self.query_job_id;
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                query_job_id,
                "Couldn't cancel the query job of a dropped streaming search outside a runtime."
            );
            return;
        };
        tracing::info!(
            query_job_id,
            "A streaming search was dropped before its query job terminated; cancelling the query \
             job."
        );
        runtime.spawn(cancel_and_wait(
            self.query_job_table.clone(),
            query_job_id,
            outcome,
        ));
    }
}

/// The delay between two consecutive polls of a streaming search's query job status.
const JOB_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Requests the cancellation of the query job of a dropped streaming search, then waits for the
/// search's outcome, logging each step's result.
///
/// # Type Parameters
///
/// * `QueryJobTableType` - The query jobs table that the search was submitted to.
async fn cancel_and_wait<QueryJobTableType: QueryJobTable>(
    query_job_table: QueryJobTableType,
    query_job_id: QueryJobId,
    outcome: OutcomeFuture,
) {
    match query_job_table.cancel(query_job_id).await {
        Ok(true) => tracing::info!(query_job_id, "Marked the query job as cancelling."),
        Ok(false) => tracing::info!(
            query_job_id,
            "The query job is neither pending nor running, so it wasn't marked as cancelling."
        ),
        Err(e) => tracing::error!(
            query_job_id,
            error = % e,
            "Failed to mark the query job as cancelling."
        ),
    }

    match outcome.await {
        Ok(SessionOutcome { status, stats }) => tracing::info!(
            query_job_id,
            status = ? status,
            num_results_emitted = stats.num_results_emitted,
            num_results_discarded = stats.num_results_discarded,
            num_duplicates_dropped = stats.num_duplicates_dropped,
            num_protocol_errors = stats.num_protocol_errors,
            "The query job of a dropped streaming search terminated."
        ),
        Err(e) => tracing::warn!(
            query_job_id,
            error = % e,
            "Failed to wait for the query job of a dropped streaming search to terminate."
        ),
    }
}
