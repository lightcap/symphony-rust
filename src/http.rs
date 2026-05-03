use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::model::{CodexTotals, RetryEntry, RunningEntry};
use crate::orchestrator::OrchestratorHandle;

#[derive(Clone)]
struct HttpState {
    orchestrator: OrchestratorHandle,
}

#[derive(Serialize)]
struct Counts {
    running: usize,
    retrying: usize,
}

#[derive(Serialize)]
struct StateResponse {
    generated_at: String,
    counts: Counts,
    running: Vec<RunningEntry>,
    retrying: Vec<RetryEntry>,
    codex_totals: CodexTotals,
    rate_limits: Option<serde_json::Value>,
    last_error: Option<String>,
}

#[derive(Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
}

pub async fn serve(
    orchestrator: OrchestratorHandle,
    port: u16,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let state = HttpState { orchestrator };
    let app = Router::new()
        .route("/", get(dashboard))
        .route("/api/v1/state", get(state_api))
        .route("/api/v1/refresh", post(refresh_api))
        .route("/api/v1/{issue_identifier}", get(issue_api))
        .with_state(state);

    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let addr = listener.local_addr()?;
    info!(addr = %addr, "http_server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await?;
    Ok(())
}

async fn dashboard(State(state): State<HttpState>) -> Html<String> {
    let snapshot = state.orchestrator.snapshot().await;
    let body = format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Symphony</title>
  <style>
    body {{ font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; margin: 2rem; color: #17202a; background: #f8fafc; }}
    main {{ max-width: 960px; margin: 0 auto; }}
    section {{ background: white; border: 1px solid #e2e8f0; border-radius: 12px; padding: 1rem 1.25rem; margin: 1rem 0; box-shadow: 0 1px 2px rgba(15, 23, 42, 0.04); }}
    code {{ background: #eef2ff; padding: 0.1rem 0.3rem; border-radius: 4px; }}
    .counts {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(160px, 1fr)); gap: 1rem; }}
    .count {{ font-size: 2rem; font-weight: 700; }}
  </style>
</head>
<body>
  <main>
    <h1>Symphony</h1>
    <section class="counts">
      <div><div class="count">{}</div><div>Running</div></div>
      <div><div class="count">{}</div><div>Retrying</div></div>
      <div><div class="count">{}</div><div>Total Tokens</div></div>
    </section>
    <section>
      <h2>API</h2>
      <p>Current JSON state is available at <code>/api/v1/state</code>.</p>
    </section>
  </main>
</body>
</html>"#,
        snapshot.running.len(),
        snapshot.retrying.len(),
        snapshot.codex_totals.total_tokens
    );
    Html(body)
}

async fn state_api(State(state): State<HttpState>) -> Json<StateResponse> {
    let snapshot = state.orchestrator.snapshot().await;
    Json(StateResponse {
        generated_at: Utc::now().to_rfc3339(),
        counts: Counts {
            running: snapshot.running.len(),
            retrying: snapshot.retrying.len(),
        },
        running: snapshot.running.into_values().collect(),
        retrying: snapshot.retrying.into_values().collect(),
        codex_totals: snapshot.codex_totals,
        rate_limits: snapshot.codex_rate_limits,
        last_error: snapshot.last_error,
    })
}

async fn issue_api(
    Path(issue_identifier): Path<String>,
    State(state): State<HttpState>,
) -> impl IntoResponse {
    let snapshot = state.orchestrator.snapshot().await;
    if let Some(entry) = snapshot
        .running
        .values()
        .find(|entry| entry.issue_identifier == issue_identifier)
    {
        return (
            StatusCode::OK,
            Json(serde_json::json!({ "running": entry })),
        )
            .into_response();
    }
    if let Some(entry) = snapshot
        .retrying
        .values()
        .find(|entry| entry.identifier == issue_identifier)
    {
        return (
            StatusCode::OK,
            Json(serde_json::json!({ "retrying": entry })),
        )
            .into_response();
    }

    json_error(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("unknown issue identifier: {issue_identifier}"),
    )
}

async fn refresh_api(State(state): State<HttpState>) -> impl IntoResponse {
    state.orchestrator.refresh();
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "accepted": true })),
    )
}

fn json_error(status: StatusCode, code: &'static str, message: String) -> axum::response::Response {
    (
        status,
        Json(ErrorEnvelope {
            error: ErrorBody { code, message },
        }),
    )
        .into_response()
}
