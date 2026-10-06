//! Test doubles shared by the listener's integration tests.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use clp_rust_utils::job_config::QueryJobId;
use clp_rust_utils::job_config::QueryJobStatus;
use search_result_listener::Error;
use search_result_listener::JobStatusSource;

/// A [`JobStatusSource`] whose status the test sets. A status of `None` stands for a job whose row
/// doesn't exist.
#[derive(Clone)]
pub struct FakeJobStatusSource {
    status: Arc<Mutex<Option<QueryJobStatus>>>,
}

impl FakeJobStatusSource {
    /// # Returns
    ///
    /// A source that reports a running job.
    pub fn new() -> Self {
        Self {
            status: Arc::new(Mutex::new(Some(QueryJobStatus::Running))),
        }
    }

    /// Sets the status the source reports from now on.
    ///
    /// # Panics
    ///
    /// Panics if the status lock is poisoned.
    pub fn set(&self, status: Option<QueryJobStatus>) {
        *self
            .status
            .lock()
            .expect("status lock shouldn't be poisoned") = status;
    }
}

#[async_trait]
impl JobStatusSource for FakeJobStatusSource {
    fn poll_interval(&self) -> Duration {
        Duration::from_millis(10)
    }

    async fn get_status(&self, query_job_id: QueryJobId) -> Result<QueryJobStatus, Error> {
        self.status
            .lock()
            .expect("status lock shouldn't be poisoned")
            .ok_or(Error::QueryJobNotFound(query_job_id))
    }
}
