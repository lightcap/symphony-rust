use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use symphony_rust::config::{ConfigManager, ServiceConfig};
use symphony_rust::env_loader::load_dotenv;
use symphony_rust::http;
use symphony_rust::orchestrator::Orchestrator;
use symphony_rust::tracker::{IssueTracker, LinearTracker};
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

    /// Validate config and tracker reads, then exit without dispatching agents.
    #[arg(long)]
    preflight: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let loaded_env_file = load_dotenv().context("failed to load .env")?;
    init_logging();
    if let Some(path) = loaded_env_file {
        info!(path = %path.display(), "env_file loaded");
    }

    let workflow_path = ConfigManager::workflow_path_from_cli(cli.workflow);
    let config = ConfigManager::load_initial(workflow_path).context("startup validation failed")?;
    let initial = config.current().await;

    let tracker = Arc::new(LinearTracker::new());
    if cli.preflight {
        run_preflight(&initial, tracker.as_ref()).await?;
        return Ok(());
    }

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

async fn run_preflight(config: &ServiceConfig, tracker: &dyn IssueTracker) -> anyhow::Result<()> {
    info!("preflight starting");
    let candidates = tracker
        .fetch_candidate_issues(config)
        .await
        .context("candidate issue fetch failed")?;
    info!(
        candidate_count = candidates.len(),
        "preflight candidates_fetched"
    );

    let terminal = tracker
        .fetch_issues_by_states(&config.tracker.terminal_states, config)
        .await
        .context("terminal issue fetch failed")?;
    info!(
        terminal_count = terminal.len(),
        "preflight terminal_issues_fetched"
    );

    let sample_ids = candidates
        .iter()
        .take(10)
        .map(|issue| issue.id.clone())
        .collect::<Vec<_>>();
    if !sample_ids.is_empty() {
        let states = tracker
            .fetch_issue_states_by_ids(&sample_ids, config)
            .await
            .context("issue state refresh failed")?;
        info!(
            state_count = states.len(),
            "preflight state_refresh_completed"
        );
    }

    info!("preflight completed");
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
