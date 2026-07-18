use anyhow::Context;
use clap::Parser;
use clp_rust_utils::{clp_config::package, database::mysql::create_clp_db_mysql_pool, serde::yaml};
use compression_coordinator::CompressionCoordinator;

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

enum ShutdownReason {
    Sigterm,
    Interrupt,
}

async fn shutdown_signal() -> ShutdownReason {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("failed to listen for SIGTERM");
    tokio::select! {
        _ = sigterm.recv() => {
            tracing::info!("Received SIGTERM.");
            ShutdownReason::Sigterm
        }
        _ = tokio::signal::ctrl_c()=> {
            tracing::info!("Forcefully shutting down.");
            ShutdownReason::Interrupt
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let (config, credentials) = read_config_and_credentials(&args)?;
    let _guard = clp_rust_utils::logging::set_up_logging("compression_coordinator.log");

    let jobs_poll_delay = config.compression_scheduler.jobs_poll_delay;

    let mut received_sigterm = false;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    let db_pool = create_clp_db_mysql_pool(&config.database, &credentials.database, 1).await?;
    let mut coordinator = CompressionCoordinator::new(db_pool);
    tracing::info!("Compression coordinator started");

    // TODO: poll existing dataset names
    loop {
        if !received_sigterm {
            tracing::debug!("Polling for pending compression jobs...");
            coordinator.search_and_schedule_new_tasks().await?;
        }

        let has_running_jobs = coordinator.poll_running_jobs().await;
        if received_sigterm && !has_running_jobs {
            tracing::info!("Received SIGTERM and there are no more running jobs. Exiting.");
            break;
        }

        tokio::select! {
            reason = &mut shutdown, if !received_sigterm => {
                match reason {
                    ShutdownReason::Sigterm => received_sigterm = true,
                    ShutdownReason::Interrupt => break,
                }
            }
            _ = tokio::time::sleep(jobs_poll_delay) => {}
        }
    }

    Ok(())
}
