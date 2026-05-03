use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use tokio::sync::{RwLock, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, sleep};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::agent::{AgentError, AgentUpdate, CodexSession, continuation_prompt};
use crate::config::{ConfigManager, ServiceConfig};
use crate::model::{
    CodexTotals, Issue, LiveSession, RetryEntry, RunningEntry, RuntimeState, normalize_state,
    state_in,
};
use crate::prompt::render_prompt;
use crate::tracker::{IssueTracker, TrackerError};
use crate::workspace::{WorkspaceManager, run_after_run, run_before_run};

const CONTINUATION_RETRY_MS: u64 = 1_000;
const RETRY_BASE_MS: u64 = 10_000;

#[derive(Clone)]
pub struct OrchestratorHandle {
    state: Arc<RwLock<SchedulerState>>,
    event_tx: mpsc::UnboundedSender<OrchestratorEvent>,
}

impl OrchestratorHandle {
    pub async fn snapshot(&self) -> RuntimeState {
        self.state.read().await.snapshot()
    }

    pub fn refresh(&self) {
        let _ = self.event_tx.send(OrchestratorEvent::Refresh);
    }
}

pub struct Orchestrator {
    config: ConfigManager,
    tracker: Arc<dyn IssueTracker>,
    state: Arc<RwLock<SchedulerState>>,
    event_tx: mpsc::UnboundedSender<OrchestratorEvent>,
    event_rx: mpsc::UnboundedReceiver<OrchestratorEvent>,
}

#[derive(Default)]
struct SchedulerState {
    poll_interval_ms: u64,
    max_concurrent_agents: usize,
    max_retry_backoff_ms: u64,
    running: HashMap<String, RunningEntry>,
    claimed: HashSet<String>,
    retrying: HashMap<String, RetryEntry>,
    retry_handles: HashMap<String, JoinHandle<()>>,
    running_controls: HashMap<String, CancellationToken>,
    completed: HashSet<String>,
    codex_totals: CodexTotals,
    ended_session_seconds: f64,
    codex_rate_limits: Option<serde_json::Value>,
    last_error: Option<String>,
}

impl SchedulerState {
    fn snapshot(&self) -> RuntimeState {
        let mut totals = self.codex_totals.clone();
        totals.seconds_running = self.ended_session_seconds
            + self
                .running
                .values()
                .map(|entry| elapsed_seconds(entry.started_at))
                .sum::<f64>();

        RuntimeState {
            poll_interval_ms: self.poll_interval_ms,
            max_concurrent_agents: self.max_concurrent_agents,
            running: self.running.clone(),
            claimed: self.claimed.iter().cloned().collect(),
            retrying: self.retrying.clone(),
            completed: self.completed.iter().cloned().collect(),
            codex_totals: totals,
            codex_rate_limits: self.codex_rate_limits.clone(),
            last_error: self.last_error.clone(),
        }
    }
}

#[derive(Debug)]
enum OrchestratorEvent {
    AgentUpdate(AgentUpdate),
    WorkerFinished(WorkerFinished),
    RetryDue(String),
    Refresh,
}

#[derive(Debug)]
struct WorkerFinished {
    issue_id: String,
    issue_identifier: String,
    attempt: Option<u32>,
    started_at: DateTime<Utc>,
    status: WorkerStatus,
}

#[derive(Debug)]
enum WorkerStatus {
    Succeeded,
    Failed(String),
    Cancelled,
}

impl Orchestrator {
    pub fn new(
        config: ConfigManager,
        tracker: Arc<dyn IssueTracker>,
    ) -> (Self, OrchestratorHandle) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let state = Arc::new(RwLock::new(SchedulerState::default()));
        let handle = OrchestratorHandle {
            state: state.clone(),
            event_tx: event_tx.clone(),
        };
        (
            Self {
                config,
                tracker,
                state,
                event_tx,
                event_rx,
            },
            handle,
        )
    }

    pub async fn run(mut self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let initial_config = self.config.current().await;
        self.apply_config_to_state(&initial_config).await;
        self.startup_terminal_cleanup(&initial_config).await;
        self.tick().await;

        let mut interval =
            tokio::time::interval(Duration::from_millis(initial_config.polling.interval_ms));
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    self.cancel_all_running().await;
                    break;
                }
                _ = interval.tick() => {
                    self.tick().await;
                    let config = self.config.current().await;
                    if config.polling.interval_ms != self.state.read().await.poll_interval_ms {
                        interval = tokio::time::interval(Duration::from_millis(config.polling.interval_ms));
                        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
                    }
                }
                Some(event) = self.event_rx.recv() => {
                    self.handle_event(event).await;
                }
            }
        }

        Ok(())
    }

    async fn tick(&self) {
        let config_for_reconcile = self.config.current().await;
        self.reconcile(&config_for_reconcile).await;

        let config = match self.config.refresh_for_dispatch().await {
            Ok(config) => config,
            Err(err) => {
                self.set_last_error(Some(err.to_string())).await;
                warn!(error = %err, "dispatch_preflight failed");
                return;
            }
        };
        self.apply_config_to_state(&config).await;

        match self.tracker.fetch_candidate_issues(&config).await {
            Ok(mut issues) => {
                sort_candidates(&mut issues);
                self.dispatch_candidates(issues, &config).await;
                self.set_last_error(None).await;
            }
            Err(err) => {
                self.set_last_error(Some(err.to_string())).await;
                warn!(error = %err, "candidate_fetch failed");
            }
        }
    }

    async fn handle_event(&self, event: OrchestratorEvent) {
        match event {
            OrchestratorEvent::AgentUpdate(update) => self.apply_agent_update(update).await,
            OrchestratorEvent::WorkerFinished(finished) => {
                self.handle_worker_finished(finished).await
            }
            OrchestratorEvent::RetryDue(issue_id) => self.handle_retry_due(issue_id).await,
            OrchestratorEvent::Refresh => self.tick().await,
        }
    }

    async fn dispatch_candidates(&self, issues: Vec<Issue>, config: &ServiceConfig) {
        for issue in issues {
            let eligible = {
                let state = self.state.read().await;
                candidate_eligible(&state, &issue, config, None)
            };

            if !eligible {
                continue;
            }

            self.dispatch_issue(issue, None, config).await;
        }
    }

    async fn dispatch_issue(&self, issue: Issue, attempt: Option<u32>, config: &ServiceConfig) {
        let workspace_manager = WorkspaceManager::new(config.workspace.root.clone());
        let workspace_path = match workspace_manager.workspace_path(&issue.identifier) {
            Ok((_, path)) => path,
            Err(err) => {
                warn!(issue_id = %issue.id, issue_identifier = %issue.identifier, error = %err, "workspace_path failed");
                return;
            }
        };
        let cancel = CancellationToken::new();
        let started_at = Utc::now();

        {
            let mut state = self.state.write().await;
            if state.claimed.contains(&issue.id) || state.running.contains_key(&issue.id) {
                return;
            }
            state.claimed.insert(issue.id.clone());
            state
                .running_controls
                .insert(issue.id.clone(), cancel.clone());
            state.running.insert(
                issue.id.clone(),
                RunningEntry {
                    issue_id: issue.id.clone(),
                    issue_identifier: issue.identifier.clone(),
                    issue: issue.clone(),
                    attempt,
                    workspace_path: workspace_path.clone(),
                    started_at,
                    live_session: LiveSession::default(),
                },
            );
        }

        info!(issue_id = %issue.id, issue_identifier = %issue.identifier, attempt = ?attempt, "dispatch completed");

        let event_tx = self.event_tx.clone();
        let config = self.config.clone();
        let tracker = self.tracker.clone();
        tokio::spawn(async move {
            let finished = run_worker(
                issue,
                attempt,
                started_at,
                config,
                tracker,
                event_tx.clone(),
                cancel,
            )
            .await;
            let _ = event_tx.send(OrchestratorEvent::WorkerFinished(finished));
        });
    }

    async fn handle_worker_finished(&self, finished: WorkerFinished) {
        let mut state = self.state.write().await;
        if state.running.remove(&finished.issue_id).is_none() {
            return;
        }
        state.running_controls.remove(&finished.issue_id);
        state.ended_session_seconds += elapsed_seconds(finished.started_at);

        match finished.status {
            WorkerStatus::Succeeded => {
                state.completed.insert(finished.issue_id.clone());
                schedule_retry_locked(
                    &mut state,
                    self.event_tx.clone(),
                    finished.issue_id,
                    finished.issue_identifier,
                    1,
                    CONTINUATION_RETRY_MS,
                    None,
                );
            }
            WorkerStatus::Failed(error) => {
                let attempt = finished.attempt.unwrap_or(0) + 1;
                let delay = retry_delay_ms(attempt, state.max_retry_backoff_ms());
                schedule_retry_locked(
                    &mut state,
                    self.event_tx.clone(),
                    finished.issue_id,
                    finished.issue_identifier,
                    attempt,
                    delay,
                    Some(error),
                );
            }
            WorkerStatus::Cancelled => {
                state.claimed.remove(&finished.issue_id);
            }
        }
    }

    async fn handle_retry_due(&self, issue_id: String) {
        let entry = {
            let mut state = self.state.write().await;
            state.retry_handles.remove(&issue_id);
            state.retrying.remove(&issue_id)
        };
        let Some(entry) = entry else {
            return;
        };

        let config = match self.config.refresh_for_dispatch().await {
            Ok(config) => config,
            Err(err) => {
                let mut state = self.state.write().await;
                schedule_retry_locked(
                    &mut state,
                    self.event_tx.clone(),
                    entry.issue_id,
                    entry.identifier,
                    entry.attempt,
                    CONTINUATION_RETRY_MS,
                    Some(err.to_string()),
                );
                return;
            }
        };
        self.apply_config_to_state(&config).await;

        match self.tracker.fetch_candidate_issues(&config).await {
            Ok(mut candidates) => {
                sort_candidates(&mut candidates);
                let issue = candidates
                    .into_iter()
                    .find(|candidate| candidate.id == entry.issue_id);
                match issue {
                    Some(issue) => {
                        let eligible = {
                            let state = self.state.read().await;
                            candidate_eligible(&state, &issue, &config, Some(&entry.issue_id))
                        };
                        if eligible {
                            {
                                let mut state = self.state.write().await;
                                state.claimed.remove(&entry.issue_id);
                            }
                            self.dispatch_issue(issue, Some(entry.attempt), &config)
                                .await;
                        } else if self
                            .has_available_slot_for_state(&issue.state, &config)
                            .await
                        {
                            self.release_claim(&entry.issue_id).await;
                        } else {
                            let mut state = self.state.write().await;
                            schedule_retry_locked(
                                &mut state,
                                self.event_tx.clone(),
                                entry.issue_id,
                                entry.identifier,
                                entry.attempt,
                                CONTINUATION_RETRY_MS,
                                Some("no available orchestrator slots".to_string()),
                            );
                        }
                    }
                    None => self.release_claim(&entry.issue_id).await,
                }
            }
            Err(err) => {
                let mut state = self.state.write().await;
                let delay = retry_delay_ms(entry.attempt, state.max_retry_backoff_ms());
                schedule_retry_locked(
                    &mut state,
                    self.event_tx.clone(),
                    entry.issue_id,
                    entry.identifier,
                    entry.attempt,
                    delay,
                    Some(err.to_string()),
                );
            }
        }
    }

    async fn reconcile(&self, config: &ServiceConfig) {
        self.reconcile_stalls(config).await;

        let ids = {
            let state = self.state.read().await;
            state.running.keys().cloned().collect::<Vec<_>>()
        };
        if ids.is_empty() {
            return;
        }

        let states = match self.tracker.fetch_issue_states_by_ids(&ids, config).await {
            Ok(states) => states,
            Err(err) => {
                warn!(error = %err, "running_state_refresh failed");
                return;
            }
        };

        for issue_id in ids {
            let Some(issue_state) = states.get(&issue_id) else {
                continue;
            };
            if state_in(&issue_state.state, &config.tracker.terminal_states) {
                self.terminate_running(
                    &issue_id,
                    TerminationAction::ReleaseAndCleanup,
                    config,
                    Some("terminal state".to_string()),
                )
                .await;
            } else if state_in(&issue_state.state, &config.tracker.active_states) {
                let mut state = self.state.write().await;
                if let Some(entry) = state.running.get_mut(&issue_id) {
                    entry.issue.state = issue_state.state.clone();
                }
            } else {
                self.terminate_running(
                    &issue_id,
                    TerminationAction::ReleaseOnly,
                    config,
                    Some("non-active state".to_string()),
                )
                .await;
            }
        }
    }

    async fn reconcile_stalls(&self, config: &ServiceConfig) {
        if config.codex.stall_timeout_ms <= 0 {
            return;
        }
        let timeout_ms = config.codex.stall_timeout_ms;
        let stalled = {
            let state = self.state.read().await;
            state
                .running
                .values()
                .filter_map(|entry| {
                    let since = entry
                        .live_session
                        .last_codex_timestamp
                        .unwrap_or(entry.started_at);
                    let elapsed = Utc::now().signed_duration_since(since).num_milliseconds();
                    (elapsed > timeout_ms).then(|| entry.issue_id.clone())
                })
                .collect::<Vec<_>>()
        };

        for issue_id in stalled {
            self.terminate_running(
                &issue_id,
                TerminationAction::Retry("stalled".to_string()),
                config,
                Some("stalled".to_string()),
            )
            .await;
        }
    }

    async fn terminate_running(
        &self,
        issue_id: &str,
        action: TerminationAction,
        config: &ServiceConfig,
        reason: Option<String>,
    ) {
        let removed = {
            let mut state = self.state.write().await;
            if let Some(cancel) = state.running_controls.remove(issue_id) {
                cancel.cancel();
            }
            let entry = state.running.remove(issue_id);
            if let Some(entry) = &entry {
                state.ended_session_seconds += elapsed_seconds(entry.started_at);
            }
            entry
        };

        let Some(entry) = removed else {
            return;
        };
        let log_issue_id = entry.issue_id.clone();
        let log_issue_identifier = entry.issue_identifier.clone();

        match action {
            TerminationAction::Retry(error) => {
                let mut state = self.state.write().await;
                let attempt = entry.attempt.unwrap_or(0) + 1;
                let delay = retry_delay_ms(attempt, state.max_retry_backoff_ms());
                schedule_retry_locked(
                    &mut state,
                    self.event_tx.clone(),
                    entry.issue_id.clone(),
                    entry.issue_identifier.clone(),
                    attempt,
                    delay,
                    Some(error),
                );
            }
            TerminationAction::ReleaseAndCleanup => {
                self.release_claim(&entry.issue_id).await;
                let manager = WorkspaceManager::new(config.workspace.root.clone());
                if let Err(err) = manager
                    .remove_workspace(&entry.issue_identifier, &config.hooks)
                    .await
                {
                    warn!(issue_id = %entry.issue_id, issue_identifier = %entry.issue_identifier, error = %err, "workspace_cleanup failed");
                }
            }
            TerminationAction::ReleaseOnly => self.release_claim(&entry.issue_id).await,
        }

        info!(issue_id = %log_issue_id, issue_identifier = %log_issue_identifier, reason = ?reason, "running_session terminated");
    }

    async fn startup_terminal_cleanup(&self, config: &ServiceConfig) {
        match self
            .tracker
            .fetch_issues_by_states(&config.tracker.terminal_states, config)
            .await
        {
            Ok(issues) => {
                let manager = WorkspaceManager::new(config.workspace.root.clone());
                for issue in issues {
                    if let Err(err) = manager
                        .remove_workspace(&issue.identifier, &config.hooks)
                        .await
                    {
                        warn!(issue_id = %issue.id, issue_identifier = %issue.identifier, error = %err, "startup_terminal_cleanup failed");
                    }
                }
            }
            Err(err) => warn!(error = %err, "startup_terminal_cleanup skipped"),
        }
    }

    async fn apply_agent_update(&self, update: AgentUpdate) {
        let mut state = self.state.write().await;
        let mut token_delta = None;
        {
            let Some(entry) = state.running.get_mut(&update.issue_id) else {
                return;
            };

            let live = &mut entry.live_session;
            live.codex_app_server_pid = update.codex_app_server_pid.clone();
            live.last_codex_event = Some(update.event.clone());
            live.last_codex_timestamp = Some(update.timestamp);
            live.last_codex_message = update.message.clone();
            if let Some(thread_id) = update.thread_id.clone() {
                live.thread_id = Some(thread_id);
            }
            if let Some(turn_id) = update.turn_id.clone() {
                live.turn_id = Some(turn_id);
            }
            if let (Some(thread_id), Some(turn_id)) = (&live.thread_id, &live.turn_id) {
                live.session_id = Some(format!("{thread_id}-{turn_id}"));
            }
            if update.event == "turn_started" {
                live.turn_count += 1;
            }
            if let Some(usage) = update.usage
                && usage.absolute
            {
                let input_delta = (usage.input_tokens - live.last_reported_input_tokens).max(0);
                let output_delta = (usage.output_tokens - live.last_reported_output_tokens).max(0);
                let total_delta = (usage.total_tokens - live.last_reported_total_tokens).max(0);
                token_delta = Some((input_delta, output_delta, total_delta));
                live.last_reported_input_tokens = usage.input_tokens;
                live.last_reported_output_tokens = usage.output_tokens;
                live.last_reported_total_tokens = usage.total_tokens;
                live.codex_input_tokens = usage.input_tokens;
                live.codex_output_tokens = usage.output_tokens;
                live.codex_total_tokens = usage.total_tokens;
            }
        }
        if let Some((input_delta, output_delta, total_delta)) = token_delta {
            state.codex_totals.input_tokens += input_delta;
            state.codex_totals.output_tokens += output_delta;
            state.codex_totals.total_tokens += total_delta;
        }
        if let Some(rate_limits) = update.rate_limits {
            state.codex_rate_limits = Some(rate_limits);
        }
    }

    async fn has_available_slot_for_state(&self, state_name: &str, config: &ServiceConfig) -> bool {
        let state = self.state.read().await;
        has_global_slot(&state, config) && has_state_slot(&state, state_name, config)
    }

    async fn release_claim(&self, issue_id: &str) {
        let mut state = self.state.write().await;
        state.claimed.remove(issue_id);
        state.retrying.remove(issue_id);
        if let Some(handle) = state.retry_handles.remove(issue_id) {
            handle.abort();
        }
    }

    async fn set_last_error(&self, error: Option<String>) {
        self.state.write().await.last_error = error;
    }

    async fn apply_config_to_state(&self, config: &ServiceConfig) {
        let mut state = self.state.write().await;
        state.poll_interval_ms = config.polling.interval_ms;
        state.max_concurrent_agents = config.agent.max_concurrent_agents;
        state.max_retry_backoff_ms = config.agent.max_retry_backoff_ms;
    }

    async fn cancel_all_running(&self) {
        let mut state = self.state.write().await;
        for (_, cancel) in state.running_controls.drain() {
            cancel.cancel();
        }
        for (_, handle) in state.retry_handles.drain() {
            handle.abort();
        }
    }
}

enum TerminationAction {
    Retry(String),
    ReleaseAndCleanup,
    ReleaseOnly,
}

impl SchedulerState {
    fn max_retry_backoff_ms(&self) -> u64 {
        if self.max_retry_backoff_ms == 0 {
            300_000
        } else {
            self.max_retry_backoff_ms
        }
    }
}

async fn run_worker(
    issue: Issue,
    attempt: Option<u32>,
    started_at: DateTime<Utc>,
    config: ConfigManager,
    tracker: Arc<dyn IssueTracker>,
    event_tx: mpsc::UnboundedSender<OrchestratorEvent>,
    cancel: CancellationToken,
) -> WorkerFinished {
    let mut workspace_path = None;
    let issue_id = issue.id.clone();
    let issue_identifier = issue.identifier.clone();

    let status = match run_worker_inner(
        &issue,
        attempt,
        &config,
        tracker,
        event_tx.clone(),
        &cancel,
        &mut workspace_path,
    )
    .await
    {
        Ok(()) => WorkerStatus::Succeeded,
        Err(AgentError::Cancelled) => WorkerStatus::Cancelled,
        Err(err) => WorkerStatus::Failed(err.to_string()),
    };

    WorkerFinished {
        issue_id,
        issue_identifier,
        attempt,
        started_at,
        status,
    }
}

async fn run_worker_inner(
    issue: &Issue,
    attempt: Option<u32>,
    config_manager: &ConfigManager,
    tracker: Arc<dyn IssueTracker>,
    orchestrator_tx: mpsc::UnboundedSender<OrchestratorEvent>,
    cancel: &CancellationToken,
    workspace_path_out: &mut Option<std::path::PathBuf>,
) -> Result<(), AgentError> {
    let config = config_manager.current().await;
    let manager = WorkspaceManager::new(config.workspace.root.clone());
    let workspace = manager
        .ensure_workspace(&issue.identifier, &config.hooks)
        .await
        .map_err(|err| AgentError::StartupFailed(err.to_string()))?;
    *workspace_path_out = Some(workspace.path.clone());

    let result = async {
        let prompt = render_prompt(&config.prompt_template, issue, attempt)
            .map_err(|err| AgentError::StartupFailed(err.to_string()))?;
        run_before_run(&workspace.path, &config.hooks)
            .await
            .map_err(|err| AgentError::StartupFailed(err.to_string()))?;

        let (agent_update_tx, mut agent_update_rx) = mpsc::unbounded_channel();
        let forward_tx = orchestrator_tx.clone();
        tokio::spawn(async move {
            while let Some(update) = agent_update_rx.recv().await {
                let _ = forward_tx.send(OrchestratorEvent::AgentUpdate(update));
            }
        });

        let mut session = CodexSession::start(
            issue.id.clone(),
            issue.identifier.clone(),
            config.clone(),
            workspace.path.clone(),
            agent_update_tx,
            cancel,
        )
        .await?;

        for turn_index in 0..config.agent.max_turns {
            let input = if turn_index == 0 {
                prompt.clone()
            } else {
                continuation_prompt()
            };
            session.run_turn(input, cancel).await?;

            if turn_index + 1 >= config.agent.max_turns {
                break;
            }

            match tracker
                .fetch_issue_states_by_ids(std::slice::from_ref(&issue.id), &config)
                .await
            {
                Ok(states) => {
                    let Some(issue_state) = states.get(&issue.id) else {
                        break;
                    };
                    if !state_in(&issue_state.state, &config.tracker.active_states) {
                        break;
                    }
                }
                Err(err) => {
                    session.shutdown().await;
                    return Err(AgentError::TurnFailed(err.to_string()));
                }
            }
        }

        session.shutdown().await;
        Ok(())
    }
    .await;

    run_after_run(&workspace.path, &config.hooks).await;
    result
}

fn candidate_eligible(
    state: &SchedulerState,
    issue: &Issue,
    config: &ServiceConfig,
    allowed_claim: Option<&str>,
) -> bool {
    if !issue.has_required_fields() {
        return false;
    }
    if !state_in(&issue.state, &config.tracker.active_states)
        || state_in(&issue.state, &config.tracker.terminal_states)
    {
        return false;
    }
    if state.running.contains_key(&issue.id) {
        return false;
    }
    if state.claimed.contains(&issue.id) && allowed_claim != Some(issue.id.as_str()) {
        return false;
    }
    if !has_global_slot(state, config) || !has_state_slot(state, &issue.state, config) {
        return false;
    }
    if normalize_state(&issue.state) == "todo" {
        return issue.blocked_by.iter().all(|blocker| {
            blocker
                .state
                .as_deref()
                .is_some_and(|state| state_in(state, &config.tracker.terminal_states))
        });
    }

    true
}

fn has_global_slot(state: &SchedulerState, config: &ServiceConfig) -> bool {
    state.running.len() < config.agent.max_concurrent_agents
}

fn has_state_slot(state: &SchedulerState, state_name: &str, config: &ServiceConfig) -> bool {
    let normalized = normalize_state(state_name);
    let limit = config
        .agent
        .max_concurrent_agents_by_state
        .get(&normalized)
        .copied()
        .unwrap_or(config.agent.max_concurrent_agents);
    let running_in_state = state
        .running
        .values()
        .filter(|entry| normalize_state(&entry.issue.state) == normalized)
        .count();
    running_in_state < limit
}

fn sort_candidates(issues: &mut [Issue]) {
    issues.sort_by(|a, b| {
        let priority = a
            .priority
            .unwrap_or(i64::MAX)
            .cmp(&b.priority.unwrap_or(i64::MAX));
        if priority != Ordering::Equal {
            return priority;
        }

        let created_at = a
            .created_at
            .map(|time| time.timestamp_millis())
            .unwrap_or(i64::MAX)
            .cmp(
                &b.created_at
                    .map(|time| time.timestamp_millis())
                    .unwrap_or(i64::MAX),
            );
        if created_at != Ordering::Equal {
            return created_at;
        }

        a.identifier.cmp(&b.identifier)
    });
}

fn schedule_retry_locked(
    state: &mut SchedulerState,
    event_tx: mpsc::UnboundedSender<OrchestratorEvent>,
    issue_id: String,
    identifier: String,
    attempt: u32,
    delay_ms: u64,
    error: Option<String>,
) {
    if let Some(handle) = state.retry_handles.remove(&issue_id) {
        handle.abort();
    }

    state.claimed.insert(issue_id.clone());
    let due_at = Utc::now() + chrono::Duration::milliseconds(delay_ms as i64);
    let due_at_ms = monotonicish_now_ms() + delay_ms as u128;
    state.retrying.insert(
        issue_id.clone(),
        RetryEntry {
            issue_id: issue_id.clone(),
            identifier: identifier.clone(),
            attempt,
            due_at,
            due_at_ms,
            error,
        },
    );

    let timer_issue_id = issue_id.clone();
    let handle = tokio::spawn(async move {
        sleep(Duration::from_millis(delay_ms)).await;
        let _ = event_tx.send(OrchestratorEvent::RetryDue(timer_issue_id));
    });
    state.retry_handles.insert(issue_id, handle);
}

fn retry_delay_ms(attempt: u32, max_retry_backoff_ms: u64) -> u64 {
    let exponent = attempt.saturating_sub(1).min(31);
    let delay = RETRY_BASE_MS.saturating_mul(2_u64.saturating_pow(exponent));
    delay.min(max_retry_backoff_ms)
}

fn elapsed_seconds(started_at: DateTime<Utc>) -> f64 {
    Utc::now()
        .signed_duration_since(started_at)
        .to_std()
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

fn monotonicish_now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

impl From<TrackerError> for AgentError {
    fn from(value: TrackerError) -> Self {
        AgentError::TurnFailed(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AgentConfig, CodexConfig, HooksConfig, PollingConfig, ServerConfig, TrackerConfig,
        WorkspaceConfig,
    };

    fn config() -> ServiceConfig {
        ServiceConfig {
            workflow_path: "WORKFLOW.md".into(),
            workflow_dir: ".".into(),
            workflow_modified: None,
            prompt_template: "{{ issue.title }}".to_string(),
            tracker: TrackerConfig {
                kind: Some("linear".to_string()),
                endpoint: "http://example.test".to_string(),
                api_key: Some("token".to_string()),
                project_slug: Some("project".to_string()),
                active_states: vec!["Todo".to_string(), "In Progress".to_string()],
                terminal_states: vec!["Done".to_string()],
            },
            polling: PollingConfig {
                interval_ms: 30_000,
            },
            workspace: WorkspaceConfig {
                root: "/tmp/symphony".into(),
            },
            hooks: HooksConfig {
                timeout_ms: 60_000,
                ..HooksConfig::default()
            },
            agent: AgentConfig {
                max_concurrent_agents: 1,
                max_turns: 20,
                max_retry_backoff_ms: 300_000,
                max_concurrent_agents_by_state: HashMap::new(),
            },
            codex: CodexConfig {
                command: "codex app-server".to_string(),
                approval_policy: None,
                thread_sandbox: None,
                turn_sandbox_policy: None,
                turn_timeout_ms: 1_000,
                read_timeout_ms: 1_000,
                stall_timeout_ms: 300_000,
            },
            server: ServerConfig::default(),
        }
    }

    fn issue(id: &str, priority: Option<i64>, created: Option<DateTime<Utc>>) -> Issue {
        Issue {
            id: id.to_string(),
            identifier: id.to_string(),
            title: "Title".to_string(),
            description: None,
            priority,
            state: "Todo".to_string(),
            branch_name: None,
            url: None,
            labels: Vec::new(),
            blocked_by: Vec::new(),
            created_at: created,
            updated_at: None,
        }
    }

    #[test]
    fn sorts_by_priority_created_identifier() {
        let now = Utc::now();
        let mut issues = vec![
            issue("B", Some(2), Some(now)),
            issue("A", Some(1), Some(now)),
            issue("C", None, None),
        ];

        sort_candidates(&mut issues);

        assert_eq!(issues[0].identifier, "A");
        assert_eq!(issues[2].identifier, "C");
    }

    #[test]
    fn rejects_blocked_todo_issue() {
        let mut issue = issue("A", Some(1), None);
        issue.blocked_by.push(crate::model::BlockerRef {
            id: Some("B".to_string()),
            identifier: Some("B".to_string()),
            state: Some("Todo".to_string()),
        });

        let state = SchedulerState::default();

        assert!(!candidate_eligible(&state, &issue, &config(), None));
    }

    #[test]
    fn computes_backoff() {
        assert_eq!(retry_delay_ms(1, 300_000), 10_000);
        assert_eq!(retry_delay_ms(2, 300_000), 20_000);
    }
}
