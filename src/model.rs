use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockerRef {
    pub id: Option<String>,
    pub identifier: Option<String>,
    pub state: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Issue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub description: Option<String>,
    pub priority: Option<i64>,
    pub state: String,
    pub branch_name: Option<String>,
    pub url: Option<String>,
    pub labels: Vec<String>,
    pub blocked_by: Vec<BlockerRef>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl Issue {
    pub fn has_required_fields(&self) -> bool {
        !self.id.trim().is_empty()
            && !self.identifier.trim().is_empty()
            && !self.title.trim().is_empty()
            && !self.state.trim().is_empty()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub absolute: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CodexTotals {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub seconds_running: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LiveSession {
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub codex_app_server_pid: Option<String>,
    pub last_codex_event: Option<String>,
    pub last_codex_timestamp: Option<DateTime<Utc>>,
    pub last_codex_message: Option<Value>,
    pub codex_input_tokens: i64,
    pub codex_output_tokens: i64,
    pub codex_total_tokens: i64,
    pub last_reported_input_tokens: i64,
    pub last_reported_output_tokens: i64,
    pub last_reported_total_tokens: i64,
    pub turn_count: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunningEntry {
    pub issue_id: String,
    pub issue_identifier: String,
    pub issue: Issue,
    pub attempt: Option<u32>,
    pub workspace_path: PathBuf,
    pub started_at: DateTime<Utc>,
    pub live_session: LiveSession,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetryEntry {
    pub issue_id: String,
    pub identifier: String,
    pub attempt: u32,
    pub due_at: DateTime<Utc>,
    pub due_at_ms: u128,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RuntimeState {
    pub poll_interval_ms: u64,
    pub max_concurrent_agents: usize,
    pub running: HashMap<String, RunningEntry>,
    pub claimed: Vec<String>,
    pub retrying: HashMap<String, RetryEntry>,
    pub completed: Vec<String>,
    pub codex_totals: CodexTotals,
    pub codex_rate_limits: Option<Value>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IssueState {
    pub id: String,
    pub identifier: Option<String>,
    pub state: String,
}

pub fn normalize_state(state: &str) -> String {
    state.to_lowercase()
}

pub fn state_in(state: &str, states: &[String]) -> bool {
    let normalized = normalize_state(state);
    states
        .iter()
        .any(|candidate| normalize_state(candidate) == normalized)
}
