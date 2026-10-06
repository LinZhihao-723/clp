//! A listener that receives the search results `clp-s` search tasks stream over TCP, and delivers
//! each result of a query job to its consumer exactly once, even when Spider reruns tasks.

mod connection;
mod cursor;
mod error;
mod job_status;
mod listener;
mod protocol;
mod session;

pub use error::Error;
pub use job_status::JobStatusSource;
pub use job_status::MariaDbJobStatusSource;
pub use listener::ListenerConfig;
pub use listener::ResultListener;
pub use session::OutcomeFuture;
pub use session::ResultStream;
pub use session::SearchResult;
pub use session::Session;
pub use session::SessionConfig;
pub use session::SessionOutcome;
pub use session::SessionStats;

/// The token that identifies a session to the search tasks streaming results to it.
pub type SessionToken = uuid::Uuid;
