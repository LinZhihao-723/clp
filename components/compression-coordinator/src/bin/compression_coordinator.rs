use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use clp_rust_utils::{clp_config::package, database::mysql::create_clp_db_mysql_pool, serde::yaml};
use compression_coordinator::CompressionCoordinator;
use const_format::formatcp;
use spider_client::SpiderClient;
use spider_core::types::id::ResourceGroupId;
use sqlx::MySqlPool;
use tokio_util::sync::CancellationToken;
use tonic::transport::Endpoint;

const SPIDER_RESOURCE_GROUP_TABLE_NAME: &str = "spider_resource_groups";

#[derive(Parser)]
#[command(version, about = "compression-coordinator for CLP.")]
struct Args {
    /// Path to the CLP config file.
    #[arg(long)]
    config: String,
}

fn read_config_and_credentials(
    args: &Args,
) -> anyhow::Result<(package::config::Config, package::credentials::Credentials)> {
    let config_path = std::path::Path::new(args.config.as_str());
    let config: package::config::Config = yaml::from_path(config_path).context(format!(
        "Failed to load config file {}",
        config_path.display()
    ))?;

    let credentials = package::credentials::Credentials {
        database: package::credentials::Database {
            password: secrecy::SecretString::new(
                std::env::var("CLP_DB_PASS")
                    .context("Expect `CLP_DB_PASS` env variable")?
                    .into_boxed_str(),
            ),
            user: std::env::var("CLP_DB_USER").context("Expect `CLP_DB_USER` env variable")?,
        },
    };
    Ok((config, credentials))
}

async fn shutdown_signal() {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("failed to listen for SIGTERM");
    tokio::select! {
        _ = sigterm.recv() => {
            tracing::info!("Received SIGTERM.");
        }
        _ = tokio::signal::ctrl_c()=> {
            tracing::info!("Forcefully shutting down.");
        }
    }
}

async fn get_or_create_resource_group_id(
    config: &package::config::CompressionCoordinator,
    spider_client: &SpiderClient,
    db_pool: &MySqlPool,
) -> anyhow::Result<ResourceGroupId> {
    const CREATE_TABLE_QUERY: &str = formatcp!(
        "CREATE TABLE IF NOT EXISTS `{table}` (
            `service_name` VARCHAR(255) NOT NULL,
            `spider_rg_id` BIGINT UNSIGNED NOT NULL,
            PRIMARY KEY (`service_name`) USING BTREE
        ) ROW_FORMAT=DYNAMIC",
        table = SPIDER_RESOURCE_GROUP_TABLE_NAME,
    );
    const SELECT_QUERY: &str = formatcp!(
        "SELECT `spider_rg_id` FROM `{table}` WHERE `service_name` = ?;",
        table = SPIDER_RESOURCE_GROUP_TABLE_NAME,
    );
    const INSERT_QUERY: &str = formatcp!(
        "INSERT INTO `{table}` (`service_name`, `spider_rg_id`) VALUES (?, ?);",
        table = SPIDER_RESOURCE_GROUP_TABLE_NAME,
    );

    let service_name = config.service_name.as_str();

    sqlx::query(CREATE_TABLE_QUERY)
        .execute(db_pool)
        .await
        .context(format!(
            "Failed to create table `{SPIDER_RESOURCE_GROUP_TABLE_NAME}`"
        ))?;

    let existing_row: Option<(u64,)> = sqlx::query_as(SELECT_QUERY)
        .bind(service_name)
        .fetch_optional(db_pool)
        .await
        .context(format!(
            "Failed to query the resource group ID for service `{service_name}`"
        ))?;
    if let Some((spider_rg_id,)) = existing_row {
        return Ok(ResourceGroupId::from(spider_rg_id));
    }

    let resource_group_id = spider_client
        .add_resource_group(service_name.to_owned(), service_name.as_bytes().to_vec())
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "Failed to create a Spider resource group for `{service_name}`: {error}"
            )
        })?;

    sqlx::query(INSERT_QUERY)
        .bind(service_name)
        .bind(resource_group_id.get())
        .execute(db_pool)
        .await
        .context(format!(
            "Failed to persist the resource group ID for service `{service_name}`"
        ))?;

    Ok(resource_group_id)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let (config, credentials) = read_config_and_credentials(&args)?;
    let _guard = clp_rust_utils::logging::set_up_logging("compression_coordinator.log");

    let spider_config = config.spider.as_ref().ok_or_else(|| {
        anyhow::anyhow!("`spider` must be configured for the compression coordinator")
    })?;
    let coordinator_config = config.compression_coordinator.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "`compression_coordinator` must be configured for the compression coordinator"
        )
    })?;

    let db_pool = create_clp_db_mysql_pool(&config.database, &credentials.database, 1).await?;

    let spider_host = spider_config.host.as_str();
    let spider_port = spider_config.port;
    let endpoint = Endpoint::from_shared(format!("http://{spider_host}:{spider_port}"))
        .context("Failed to parse the Spider storage endpoint as a URL")?;
    let spider_client = SpiderClient::builder(endpoint)
        .connect()
        .await
        .map_err(|error| anyhow::anyhow!("Failed to connect to the Spider cluster: {error}"))?;

    let resource_group_id =
        get_or_create_resource_group_id(coordinator_config, &spider_client, &db_pool).await?;

    // TODO: Failure recovery is missing.
    let cancellation_token = CancellationToken::new();
    let coordinator = CompressionCoordinator::new(
        db_pool,
        spider_client,
        resource_group_id,
        coordinator_config.jobs_poll_delay,
        cancellation_token.clone(),
    );

    let mut handle = tokio::spawn(coordinator.run());
    tracing::info!("Compression coordinator started");

    shutdown_signal().await;
    cancellation_token.cancel();

    let termination_timeout = Duration::from_secs(coordinator_config.termination_timeout_seconds);
    match tokio::time::timeout(termination_timeout, &mut handle).await {
        Ok(Ok(Ok(()))) => {
            tracing::info!("Compression coordinator stopped.");
            Ok(())
        }
        Ok(Ok(Err(err))) => Err(err),
        Ok(Err(err)) => Err(anyhow::anyhow!(
            "Failed to join the compression coordinator task: {err}"
        )),
        Err(_) => {
            tracing::warn!(
                "The compression coordinator did not stop within {termination_timeout:?}. \
                 Aborting."
            );
            handle.abort();
            Ok(())
        }
    }
}
