//! The `commit` task: finalizes a compression job once all its compression tasks have succeeded.

use anyhow::Context;
use clp_rust_utils::{
    clp_config::package::credentials,
    database::mysql::create_clp_db_mysql_pool,
    dataset::VALID_DATASET_NAME_REGEX,
    job_config::{CompressionJobId, CompressionJobStatus},
};
use secrecy::SecretString;
use spider_core::types::id::JobId;

use crate::task_io::ArchiveMetadata;

/// The env var holding the CLP DB user name.
const CLP_DB_USER_ENV_VAR: &str = "CLP_DB_USER";

/// The env var holding the CLP DB password.
const CLP_DB_PASS_ENV_VAR: &str = "CLP_DB_PASS";

/// Commits a compression job's `archives` and marks it succeeded.
///
/// Called by the commit-task TDL wrapper once every compression task has succeeded.
///
/// # Returns
///
/// `()` once the job's archives are inserted and the job is marked succeeded.
///
/// # Errors
///
/// Forwards [`commit_async`]'s errors.
pub fn commit(
    spider_job_id: JobId,
    dataset: Option<String>,
    archives: Vec<ArchiveMetadata>,
) -> anyhow::Result<()> {
    super::runtime().block_on(commit_async(spider_job_id, dataset, archives))
}

/// In one DB transaction, inserts all `archives` and CAS-transitions the CLP compression job (found
/// by reverse-lookup on `spider_id`) from `RUNNING` to `SUCCEEDED`, recording the job's total sizes
/// and duration.
///
/// # Returns
///
/// `()` once the transaction commits (or immediately if the job is already `SUCCEEDED`).
///
/// # Errors
///
/// Returns an error if `archives` is empty, if the DB credentials env vars are unset, if the
/// dataset name is invalid, if the CLP compression job is missing or not in the `RUNNING` state, or
/// if any DB operation fails. Forwards [`create_clp_db_mysql_pool`]'s errors.
async fn commit_async(
    spider_job_id: JobId,
    dataset: Option<String>,
    archives: Vec<ArchiveMetadata>,
) -> anyhow::Result<()> {
    if archives.is_empty() {
        anyhow::bail!("commit received no archives to publish");
    }
    let total_uncompressed_size: i64 = archives.iter().map(|a| a.uncompressed_size).sum();
    let total_compressed_size: i64 = archives.iter().map(|a| a.size).sum();

    let config = super::spider_task_executor_config();

    // The dataset is interpolated into the table name (it cannot be bound), so guard against SQL
    // injection by validating it against the allowed-name pattern.
    if let Some(dataset) = dataset.as_deref()
        && !VALID_DATASET_NAME_REGEX.is_match(dataset)
    {
        anyhow::bail!("invalid dataset name: {dataset}");
    }
    let archives_table = archives_table_name(&config.database.table_prefix, dataset.as_deref());

    let credentials = db_credentials_from_env()?;

    let pool = create_clp_db_mysql_pool(&config.database, &credentials, 1)
        .await
        .context("failed to create the CLP DB connection pool")?;

    let mut tx = pool
        .begin()
        .await
        .context("failed to begin the commit transaction")?;

    let job: Option<(CompressionJobId, CompressionJobStatus)> =
        sqlx::query_as("SELECT id, status FROM compression_jobs WHERE spider_id = ? FOR UPDATE")
            .bind(spider_job_id.get())
            .fetch_optional(&mut *tx)
            .await
            .context("failed to look up the CLP compression job")?;
    let Some((id, status)) = job else {
        anyhow::bail!(
            "no CLP compression job found for spider job {}",
            spider_job_id.get()
        );
    };

    if status == CompressionJobStatus::Succeeded {
        return Ok(());
    }
    if status != CompressionJobStatus::Running {
        anyhow::bail!(
            "CLP compression job {id} is in unexpected state {status:?}; refusing to commit"
        );
    }

    let mut builder = sqlx::QueryBuilder::<sqlx::MySql>::new(format!(
        "INSERT INTO `{archives_table}` (id, begin_timestamp, end_timestamp, uncompressed_size, \
         size, creator_id, creation_ix) "
    ));
    builder.push_values(&archives, |mut row, archive| {
        // clp-s does not emit creator_id/creation_ix; mirror Python's "" / 0 defaults.
        row.push_bind(&archive.id)
            .push_bind(archive.begin_timestamp)
            .push_bind(archive.end_timestamp)
            .push_bind(archive.uncompressed_size)
            .push_bind(archive.size)
            .push_bind("")
            .push_bind(0_i32);
    });
    builder
        .build()
        .execute(&mut *tx)
        .await
        .with_context(|| format!("failed to insert archives into `{archives_table}`"))?;

    // `duration` (seconds) is derived from the DB clock so it stays consistent with the DB's time.
    sqlx::query(
        "UPDATE compression_jobs SET status = ?, uncompressed_size = ?, compressed_size = ?, \
         duration = TIMESTAMPDIFF(MICROSECOND, start_time, CURRENT_TIMESTAMP(3)) / 1000000, \
         update_time = CURRENT_TIMESTAMP() WHERE id = ?",
    )
    .bind(CompressionJobStatus::Succeeded)
    .bind(total_uncompressed_size)
    .bind(total_compressed_size)
    .bind(id)
    .execute(&mut *tx)
    .await
    .context("failed to mark the CLP compression job succeeded")?;

    tx.commit()
        .await
        .context("failed to commit the transaction")?;
    Ok(())
}

/// Reads the CLP DB credentials from [`CLP_DB_USER_ENV_VAR`] and [`CLP_DB_PASS_ENV_VAR`].
///
/// # Returns
///
/// The CLP DB credentials.
///
/// # Errors
///
/// Returns an error if either env var is unset or not valid Unicode.
fn db_credentials_from_env() -> anyhow::Result<credentials::Database> {
    let user = std::env::var(CLP_DB_USER_ENV_VAR).with_context(|| {
        format!("failed to read the `{CLP_DB_USER_ENV_VAR}` environment variable")
    })?;
    let password = std::env::var(CLP_DB_PASS_ENV_VAR).with_context(|| {
        format!("failed to read the `{CLP_DB_PASS_ENV_VAR}` environment variable")
    })?;
    Ok(credentials::Database {
        user,
        password: SecretString::from(password),
    })
}

/// Builds the archives table name (mirror of Python's `_get_table_name`).
///
/// # Returns
///
/// `<table_prefix><dataset>_archives` when a dataset is given, otherwise `<table_prefix>archives`.
fn archives_table_name(table_prefix: &str, dataset: Option<&str>) -> String {
    dataset.map_or_else(
        || format!("{table_prefix}archives"),
        |dataset| format!("{table_prefix}{dataset}_archives"),
    )
}

#[cfg(test)]
mod tests {
    use super::archives_table_name;

    #[test]
    fn archives_table_name_with_dataset() {
        assert_eq!(
            archives_table_name("clp_", Some("mydataset")),
            "clp_mydataset_archives"
        );
    }

    #[test]
    fn archives_table_name_without_dataset() {
        assert_eq!(archives_table_name("clp_", None), "clp_archives");
    }
}
