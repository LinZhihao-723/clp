//! End-to-end test harness that drives one full CLP S3 compression job on a Spider (Huntsman)
//! cluster through the [`S3CompressionJobSubmitter`] implementation for [`SpiderClient`].
//!
//! Simulating the real coordinator flow, the harness decodes the `clp-s` options, dataset, and input
//! S3 settings from the compression job's stored config blob (`compression_jobs.clp_config`, a
//! Brotli-compressed msgpack [`ClpIoConfig`]) rather than from environment variables. It creates its
//! own Spider resource group for the submission; only the CLP DB and Spider endpoint are
//! environment-driven.
//!
//! Against a loaded CLP metadata-DB snapshot, the harness:
//!
//! 1. Reads the single compression job (its id and `clp_config` blob) and its ingested S3 object
//!    keys from the CLP DB, decoding the job config to recover the `clp-s` options and input S3
//!    settings.
//! 2. Partitions the object keys into exactly [`NUM_COMPRESSION_TASKS`] compression tasks, as evenly
//!    as possible.
//! 3. Submits the job to Spider, persists the returned Spider job id, and marks the CLP job
//!    `RUNNING`.
//! 4. Ensures the target archives and column-metadata tables exist.
//! 5. Runs the job to completion and verifies the CLP job reached `SUCCEEDED` with its archives
//!    published, printing a clear `E2E PASS`/`E2E FAIL` summary.
//!
//! It never handles AWS credentials: the input's authentication method comes from the decoded job
//! config, and the executor supplies the actual S3 credentials from its own environment.
//!
//! Build/lint only (no cluster access needed):
//!
//! ```shell
//! cargo build --example e2e_compress -p compression-coordinator
//! cargo clippy --example e2e_compress -p compression-coordinator -- -D warnings
//! ```

use anyhow::{Context, bail};
use clp_rust_utils::{
    clp_config::package::config::Database,
    dataset::VALID_DATASET_NAME_REGEX,
    job_config::{ClpIoConfig, CompressionJobId, CompressionJobStatus, InputConfig},
    serde::BrotliMsgpack,
};
use compression_coordinator::{
    compression_job_submitter::{CompressionJobCompletion, S3CompressionJobSubmitter},
    task_io::{ClpSCompressionOption, S3InputSource},
};
use spider_client::SpiderClient;
use spider_core::types::id::JobId;
use sqlx::{
    MySqlPool,
    mysql::{MySqlConnectOptions, MySqlPoolOptions},
};
use tonic::transport::Endpoint;

/// The CLP metadata-DB table prefix (mirror of Python's `table_prefix` default).
const CLP_TABLE_PREFIX: &str = "clp_";

/// The exact number of compression tasks the job's objects are partitioned into.
const NUM_COMPRESSION_TASKS: usize = 10;

/// The external-id prefix of the Spider resource group the harness creates. A unique per-run suffix
/// is appended so repeated runs against the same cluster do not collide (`add_resource_group` rejects
/// a duplicate external id).
const HARNESS_RESOURCE_GROUP_EXTERNAL_ID_PREFIX: &str = "clp-e2e-harness";

/// The password of the Spider resource group the harness creates.
const HARNESS_RESOURCE_GROUP_PASSWORD: &[u8] = b"clp-e2e-harness";

/// The harness's runtime configuration. Only the CLP DB and Spider endpoint are environment-driven;
/// the `clp-s` options, dataset, and input S3 settings are decoded from the job's `clp_config` blob,
/// and the resource group is created on the Spider cluster.
struct HarnessConfig {
    db_host: String,
    db_port: u16,
    db_user: String,
    db_password: String,
    db_name: String,
    spider_endpoint: String,
}

impl HarnessConfig {
    /// Reads the harness configuration from environment variables, applying defaults.
    ///
    /// # Returns
    ///
    /// The configuration on success.
    ///
    /// # Errors
    ///
    /// Returns an error if any numeric environment variable cannot be parsed.
    fn from_env() -> anyhow::Result<Self> {
        let db_port = env_or("HARNESS_DB_PORT", "3306")
            .parse()
            .context("failed to parse HARNESS_DB_PORT as a port number")?;

        Ok(Self {
            db_host: env_or("HARNESS_DB_HOST", "127.0.0.1"),
            db_port,
            db_user: env_or("HARNESS_DB_USER", "clp-user"),
            db_password: env_or("HARNESS_DB_PASSWORD", ""),
            db_name: env_or("HARNESS_DB_NAME", "clp-db"),
            spider_endpoint: env_or("SPIDER_STORAGE_ENDPOINT", "http://172.40.0.30:50051"),
        })
    }
}

/// Reads an environment variable, falling back to `default` when unset or non-Unicode.
///
/// # Returns
///
/// The variable's value, or `default` when unset.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

/// Partitions `keys` into `num_groups` groups whose sizes differ by at most one, dropping empty
/// groups (which arise only when there are fewer keys than groups).
///
/// # Returns
///
/// The non-empty groups, in order.
fn partition_evenly(keys: Vec<String>, num_groups: usize) -> Vec<Vec<String>> {
    let base_size = keys.len() / num_groups;
    let num_larger_groups = keys.len() % num_groups;
    let mut remaining = keys.into_iter();
    let mut groups = Vec::with_capacity(num_groups);
    for group_ix in 0..num_groups {
        let group_size = base_size + usize::from(group_ix < num_larger_groups);
        if group_size == 0 {
            continue;
        }
        groups.push(remaining.by_ref().take(group_size).collect());
    }
    groups
}

/// Connects a [`MySqlPool`] to the CLP metadata DB described by `config`.
///
/// # Returns
///
/// The connection pool on success.
///
/// # Errors
///
/// Returns an error if the connection cannot be established.
async fn connect_clp_db(config: &HarnessConfig) -> anyhow::Result<MySqlPool> {
    let options = MySqlConnectOptions::new()
        .host(&config.db_host)
        .port(config.db_port)
        .username(&config.db_user)
        .password(&config.db_password)
        .database(&config.db_name);
    MySqlPoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .context("failed to connect to the CLP metadata DB")
}

/// Reads the single compression job's id and its `clp_config` blob from `compression_jobs`.
///
/// # Returns
///
/// The lone job's id and its Brotli-compressed msgpack config blob on success.
///
/// # Errors
///
/// Returns an error if the query fails or the table does not hold exactly one row.
async fn read_single_job(pool: &MySqlPool) -> anyhow::Result<(CompressionJobId, Vec<u8>)> {
    let rows: Vec<(CompressionJobId, Vec<u8>)> =
        sqlx::query_as("SELECT id, clp_config FROM compression_jobs")
            .fetch_all(pool)
            .await
            .context("failed to read the compression job")?;
    if rows.len() != 1 {
        bail!("expected exactly one compression job, found {}", rows.len());
    }
    Ok(rows
        .into_iter()
        .next()
        .expect("row count was checked to be exactly one"))
}

/// Reads the object keys of the ingested S3 objects belonging to compression job `clp_job_id`.
///
/// # Returns
///
/// The objects' keys on success.
///
/// # Errors
///
/// Returns an error if the query fails or the job has no ingested objects.
async fn read_job_object_keys(
    pool: &MySqlPool,
    clp_job_id: CompressionJobId,
) -> anyhow::Result<Vec<String>> {
    let object_keys: Vec<String> =
        sqlx::query_scalar("SELECT `key` FROM ingested_s3_object_metadata WHERE compression_job_id = ?")
            .bind(clp_job_id)
            .fetch_all(pool)
            .await
            .context("failed to read the job's ingested S3 object keys")?;
    if object_keys.is_empty() {
        bail!("compression job {clp_job_id} has no ingested S3 objects");
    }
    Ok(object_keys)
}

/// Ensures the archives and column-metadata tables for `dataset` exist, using the CLP metadata-DB
/// schema (`clp_py_utils::clp_metadata_db_utils`).
///
/// # Returns
///
/// The archives table's name on success.
///
/// # Errors
///
/// Returns an error if `dataset` is not a valid dataset name or if either `CREATE TABLE` fails.
async fn ensure_metadata_tables(pool: &MySqlPool, dataset: Option<&str>) -> anyhow::Result<String> {
    if let Some(dataset) = dataset
        && !VALID_DATASET_NAME_REGEX.is_match(dataset)
    {
        bail!("invalid dataset name: {dataset}");
    }

    let database = Database {
        table_prefix: CLP_TABLE_PREFIX.to_owned(),
        ..Database::default()
    };
    let archives_table = database.archives_table_name(dataset);
    let column_metadata_table = database.column_metadata_table_name(dataset);

    sqlx::query(&format!(
        "CREATE TABLE IF NOT EXISTS `{archives_table}` (
            `pagination_id` BIGINT unsigned NOT NULL AUTO_INCREMENT,
            `id` VARCHAR(64) NOT NULL,
            `begin_timestamp` BIGINT NOT NULL,
            `end_timestamp` BIGINT NOT NULL,
            `uncompressed_size` BIGINT NOT NULL,
            `size` BIGINT NOT NULL,
            `creator_id` VARCHAR(64) NOT NULL,
            `creation_ix` INT NOT NULL,
            KEY `archives_creation_order` (`creator_id`,`creation_ix`) USING BTREE,
            UNIQUE KEY `archive_id` (`id`) USING BTREE,
            PRIMARY KEY (`pagination_id`)
        )"
    ))
    .execute(pool)
    .await
    .with_context(|| format!("failed to create the `{archives_table}` table"))?;

    sqlx::query(&format!(
        "CREATE TABLE IF NOT EXISTS `{column_metadata_table}` (
            `name` VARCHAR(512) NOT NULL,
            `type` TINYINT NOT NULL,
            PRIMARY KEY (`name`, `type`)
        )"
    ))
    .execute(pool)
    .await
    .with_context(|| format!("failed to create the `{column_metadata_table}` table"))?;

    Ok(archives_table)
}

/// Drives the whole end-to-end compression job and prints a `PASS`/`FAIL` summary.
///
/// # Returns
///
/// `()` when the job succeeded and every verification passed.
///
/// # Errors
///
/// Returns an error if any step (DB access, config decoding, Spider submission, job execution)
/// fails, or if the final verification fails.
async fn run(config: HarnessConfig) -> anyhow::Result<()> {
    let pool = connect_clp_db(&config).await?;

    let (clp_job_id, clp_config_blob) = read_single_job(&pool).await?;
    println!("CLP compression job id: {clp_job_id}");

    let io_config: ClpIoConfig = BrotliMsgpack::deserialize(&clp_config_blob)
        .context("failed to decode the job's clp_config blob")?;
    let output = io_config.output;
    let InputConfig::S3ObjectMetadataInputConfig { config: input } = io_config.input else {
        bail!("expected an s3_object_metadata input config; the harness cannot drive an s3 input");
    };
    let s3_config = input.s3_config;
    let dataset: Option<String> = input.dataset.map(String::from);
    let clp_s_option = ClpSCompressionOption {
        target_encoded_size: output.target_segment_size + output.target_dictionaries_size,
        compression_level: i32::from(output.compression_level),
        timestamp_key: input.timestamp_key.map(String::from),
    };
    println!(
        "decoded job config: dataset={dataset:?}, bucket=`{}`, target_encoded_size={}, \
         compression_level={}",
        s3_config.bucket, clp_s_option.target_encoded_size, clp_s_option.compression_level
    );

    let object_keys = read_job_object_keys(&pool, clp_job_id).await?;
    println!("read {} ingested object key(s)", object_keys.len());

    let partitions = partition_evenly(object_keys, NUM_COMPRESSION_TASKS);
    println!(
        "partitioned into {} compression task(s) with sizes {:?}",
        partitions.len(),
        partitions.iter().map(Vec::len).collect::<Vec<_>>()
    );

    let input_sources = partitions
        .into_iter()
        .map(|object_keys| S3InputSource {
            endpoint_url: s3_config.endpoint_url.clone(),
            region_code: s3_config.region_code.clone(),
            bucket: s3_config.bucket.clone(),
            aws_authentication: s3_config.aws_authentication.clone(),
            object_keys,
        })
        .collect();

    let endpoint = Endpoint::from_shared(config.spider_endpoint.clone())
        .context("failed to parse SPIDER_STORAGE_ENDPOINT as a URL")?;
    let client = SpiderClient::builder(endpoint)
        .connect()
        .await
        .map_err(|error| anyhow::anyhow!("failed to connect to the Spider cluster: {error}"))?;

    let external_resource_group_id = format!(
        "{HARNESS_RESOURCE_GROUP_EXTERNAL_ID_PREFIX}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    );
    let resource_group_id = client
        .add_resource_group(external_resource_group_id, HARNESS_RESOURCE_GROUP_PASSWORD.to_vec())
        .await
        .map_err(|error| anyhow::anyhow!("failed to create the Spider resource group: {error}"))?;
    println!("created Spider resource group id: {}", resource_group_id.get());

    let spider_job_id: JobId = client
        .submit_s3_compression_job(resource_group_id, clp_s_option, dataset.clone(), input_sources)
        .await
        .map_err(|error| anyhow::anyhow!("failed to submit the compression job: {error}"))?;
    println!("submitted Spider job id: {}", spider_job_id.get());

    sqlx::query(
        "UPDATE compression_jobs SET spider_id = ?, status = ?, start_time = CURRENT_TIMESTAMP() \
         WHERE id = ?",
    )
        .bind(spider_job_id.get())
        .bind(CompressionJobStatus::Running)
        .bind(clp_job_id)
        .execute(&pool)
        .await
        .context("failed to persist the Spider job id and mark the CLP job running")?;

    let archives_table = ensure_metadata_tables(&pool, dataset.as_deref()).await?;
    println!("ensured metadata tables (archives table: `{archives_table}`)");

    println!("running the job to completion...");
    let completion = client
        .run_s3_compression_job_to_completion(spider_job_id)
        .await
        .map_err(|error| anyhow::anyhow!("failed to run the compression job: {error}"))?;
    let completion_succeeded = match &completion {
        CompressionJobCompletion::Succeeded => {
            println!("job completion: Succeeded");
            true
        }
        CompressionJobCompletion::Failed { error_message } => {
            println!("job completion: Failed: {error_message}");
            false
        }
        CompressionJobCompletion::Cancelled => {
            println!("job completion: Cancelled");
            false
        }
    };

    verify_and_report(&pool, clp_job_id, &archives_table, completion_succeeded).await
}

/// Verifies the CLP job reached `SUCCEEDED` with published archives and prints the `PASS`/`FAIL`
/// summary.
///
/// # Returns
///
/// `()` when every check passed.
///
/// # Errors
///
/// Returns an error if a verification query fails or if any check failed.
async fn verify_and_report(
    pool: &MySqlPool,
    clp_job_id: CompressionJobId,
    archives_table: &str,
    completion_succeeded: bool,
) -> anyhow::Result<()> {
    let (status, uncompressed_size, compressed_size, duration): (
        CompressionJobStatus,
        i64,
        i64,
        Option<f64>,
    ) = sqlx::query_as(
        "SELECT status, uncompressed_size, compressed_size, duration FROM compression_jobs \
         WHERE id = ?",
    )
    .bind(clp_job_id)
    .fetch_one(pool)
    .await
    .context("failed to read the CLP job's final state")?;

    let archives: Vec<(String, i64, i64, i64, i64)> = sqlx::query_as(&format!(
        "SELECT id, begin_timestamp, end_timestamp, uncompressed_size, size FROM `{archives_table}`"
    ))
    .fetch_all(pool)
    .await
    .with_context(|| format!("failed to read the `{archives_table}` table"))?;

    println!("=== E2E verification ===");
    println!(
        "compression_jobs: status={status:?}, uncompressed_size={uncompressed_size}, \
         compressed_size={compressed_size}, duration={duration:?}"
    );
    println!("`{archives_table}` rows ({}):", archives.len());
    for (id, begin_timestamp, end_timestamp, archive_uncompressed_size, size) in &archives {
        println!(
            "  id={id}, begin_timestamp={begin_timestamp}, end_timestamp={end_timestamp}, \
             uncompressed_size={archive_uncompressed_size}, size={size}"
        );
    }

    let status_ok = status == CompressionJobStatus::Succeeded;
    let sizes_ok = uncompressed_size > 0 && compressed_size > 0;
    let duration_ok = duration.is_some();
    let archives_ok = !archives.is_empty();
    let pass = completion_succeeded && status_ok && sizes_ok && duration_ok && archives_ok;

    if pass {
        println!("E2E PASS");
        Ok(())
    } else {
        println!(
            "E2E FAIL: completion_succeeded={completion_succeeded}, status_ok={status_ok}, \
             sizes_ok={sizes_ok}, duration_ok={duration_ok}, archives_ok={archives_ok}"
        );
        bail!("end-to-end verification failed")
    }
}

/// Builds a multi-threaded tokio runtime and runs the harness on it.
///
/// # Returns
///
/// `()` on a passing end-to-end run.
///
/// # Errors
///
/// Returns an error if the runtime cannot be built or the harness fails.
fn main() -> anyhow::Result<()> {
    let config = HarnessConfig::from_env()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build the tokio runtime")?;
    runtime.block_on(run(config))
}
