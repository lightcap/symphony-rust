use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use symphony_rust::config::ConfigManager;
use symphony_rust::env_loader::load_dotenvs;
use symphony_rust::http;
use symphony_rust::orchestrator::Orchestrator;
use symphony_rust::tracker::LinearTracker;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "symphony",
    about = "Run the Symphony coding-agent orchestrator"
)]
struct Cli {
    /// Path to WORKFLOW.md. Defaults to ./WORKFLOW.md.
    workflow: Option<PathBuf>,

    /// Enable the optional HTTP status API on this loopback port. Use 0 for an ephemeral port.
    #[arg(long)]
    port: Option<u16>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let workflow_path = ConfigManager::workflow_path_from_cli(cli.workflow);
    let loaded_env_files =
        load_dotenvs(&workflow_path).context("failed to load environment files")?;
    init_logging();
    for path in loaded_env_files {
        info!(path = %path.display(), "env_file loaded");
    }

    let config = ConfigManager::load_initial(workflow_path).context("startup validation failed")?;
    let initial = config.current().await;

    let tracker = Arc::new(LinearTracker::new());
    let (orchestrator, handle) = Orchestrator::new(config, tracker);
    let shutdown = CancellationToken::new();

    let http_port = cli.port.or(initial.server.port);
    let http_task = http_port.map(|port| {
        let http_shutdown = shutdown.clone();
        let http_handle = handle.clone();
        tokio::spawn(async move {
            if let Err(err) = http::serve(http_handle, port, http_shutdown).await {
                error!(error = %err, "http_server failed");
            }
        })
    });

    let orchestrator_shutdown = shutdown.clone();
    let mut orchestrator_task =
        tokio::spawn(async move { orchestrator.run(orchestrator_shutdown).await });
    let mut orchestrator_done = false;

    tokio::select! {
        signal = tokio::signal::ctrl_c() => {
            signal.context("failed to listen for shutdown signal")?;
            info!("shutdown_signal received");
            shutdown.cancel();
        }
        result = &mut orchestrator_task => {
            result.context("orchestrator task join failed")??;
            orchestrator_done = true;
            shutdown.cancel();
        }
    }

    if !orchestrator_done {
        orchestrator_task
            .await
            .context("orchestrator task join failed")??;
    }
    if let Some(task) = http_task {
        task.abort();
    }
    Ok(())
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("symphony_rust=info,info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .json()
        .with_current_span(false)
        .with_span_list(false)
        .init();
}
