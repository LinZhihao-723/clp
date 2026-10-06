use const_format::formatcp;
use secrecy::ExposeSecret;
use strum::IntoEnumIterator;

use crate::clp_config::package::config::Database as DatabaseConfig;
use crate::clp_config::package::credentials::Database as DatabaseCredentials;
use crate::job_config::QUERY_JOBS_TABLE_NAME;
use crate::job_config::QueryJobId;
use crate::job_config::QueryJobStatus;
use crate::job_config::QueryJobType;
use crate::job_config::SearchJobConfig;

/// Implements [`sqlx::Type<sqlx::MySql>`] for `$ty` by delegating to `$delegate`.
///
/// # Examples
///
/// ```rust
/// impl_sqlx_type!(IngestedS3ObjectMetadataStatus => str);
/// ```
#[macro_export]
macro_rules! impl_sqlx_type {
    ($ty:ty => $delegate:ty $(,)?) => {
        impl ::sqlx::Type<::sqlx::MySql> for $ty {
            fn type_info() -> <::sqlx::MySql as ::sqlx::Database>::TypeInfo {
                <$delegate as ::sqlx::Type<::sqlx::MySql>>::type_info()
            }

            fn compatible(ty: &<::sqlx::MySql as ::sqlx::Database>::TypeInfo) -> bool {
                <$delegate as ::sqlx::Type<::sqlx::MySql>>::compatible(ty)
            }
        }
    };
}

/// Trait for formatting Rust enums as SQL `ENUM(...)` declarations.
pub trait MySqlEnumFormat: IntoEnumIterator + Sized + ToString
where
    Self::Iterator: Iterator<Item = Self>, {
    /// # Returns
    ///
    /// A string representing the SQL enum definition for this enum.
    #[must_use]
    fn format_as_sql_enum() -> String {
        let inner = Self::iter()
            .map(|v| format!("'{}'", v.to_string()))
            .collect::<Vec<_>>()
            .join(", ");
        format!("ENUM({inner})")
    }
}

/// Creates a new `MySQL` connection pool to the CLP DB using the provided configuration and
/// credentials.
///
/// # Return
///
/// A newly created `MySQL` connection pool configured with the specified maximum number of
/// connections.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`sqlx::mysql::MySqlPoolOptions::connect_with`]'s errors on failure.
pub async fn create_clp_db_mysql_pool(
    config: &DatabaseConfig,
    credentials: &DatabaseCredentials,
    max_connections: u32,
) -> Result<sqlx::MySqlPool, crate::Error> {
    let mysql_options = sqlx::mysql::MySqlConnectOptions::new()
        .host(&config.host)
        .port(config.port)
        .database(&config.names.clp)
        .username(&credentials.user)
        .password(credentials.password.expose_secret());

    Ok(sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(max_connections)
        .connect_with(mysql_options)
        .await?)
}

/// Submits a search job by inserting it into the CLP DB's query jobs table.
///
/// # Returns
///
/// The ID of the submitted query job on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`crate::Error::QueryJobIdOutOfRange`] if the ID of the inserted row doesn't fit in
///   [`QueryJobId`].
/// * Forwards [`rmp_serde::to_vec_named`]'s return values on failure.
/// * Forwards [`sqlx::query::Query::execute`]'s return values on failure.
pub async fn submit_query_job(
    db_pool: &sqlx::MySqlPool,
    search_job_config: &SearchJobConfig,
) -> Result<QueryJobId, crate::Error> {
    const QUERY: &str =
        formatcp!("INSERT INTO `{QUERY_JOBS_TABLE_NAME}` (`job_config`, `type`) VALUES (?, ?)");

    let query_result = sqlx::query(QUERY)
        .bind(rmp_serde::to_vec_named(search_job_config)?)
        .bind(QueryJobType::SearchOrAggregation)
        .execute(db_pool)
        .await?;

    let query_job_id = query_result.last_insert_id();
    QueryJobId::try_from(query_job_id).map_err(|_| crate::Error::QueryJobIdOutOfRange(query_job_id))
}

/// Requests the cancellation of a query job by marking it as [`QueryJobStatus::Cancelling`], if it
/// is [`QueryJobStatus::Pending`] or [`QueryJobStatus::Running`].
///
/// # Returns
///
/// Whether the query job was marked on success. A query job that doesn't exist or is in any other
/// status isn't marked.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`sqlx::query::Query::execute`]'s return values on failure.
pub async fn cancel_query_job(
    db_pool: &sqlx::MySqlPool,
    query_job_id: QueryJobId,
) -> Result<bool, crate::Error> {
    const QUERY: &str = formatcp!(
        "UPDATE `{QUERY_JOBS_TABLE_NAME}` SET `status` = ? WHERE `id` = ? AND `status` IN (?, ?)"
    );

    let query_result = sqlx::query(QUERY)
        .bind(QueryJobStatus::Cancelling)
        .bind(query_job_id)
        .bind(QueryJobStatus::Pending)
        .bind(QueryJobStatus::Running)
        .execute(db_pool)
        .await?;
    Ok(0 != query_result.rows_affected())
}
