use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use chrono::Utc;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::config::ServiceConfig;
use crate::model::TokenUsage;
use crate::tracker::linear_graphql_tool;
use crate::workspace::{WorkspaceError, validate_agent_cwd};

#[derive(Clone, Debug)]
pub struct AgentUpdate {
    pub issue_id: String,
    pub issue_identifier: String,
    pub event: String,
    pub timestamp: chrono::DateTime<Utc>,
    pub codex_app_server_pid: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub usage: Option<TokenUsage>,
    pub rate_limits: Option<Value>,
    pub message: Option<Value>,
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("codex_not_found reason={0}")]
    CodexNotFound(String),
    #[error("invalid_workspace_cwd reason={0}")]
    InvalidWorkspaceCwd(String),
    #[error("response_timeout method={0}")]
    ResponseTimeout(String),
    #[error("turn_timeout")]
    TurnTimeout,
    #[error("port_exit")]
    PortExit,
    #[error("response_error method={method} code={code:?} message={message}")]
    ResponseError {
        method: String,
        code: Option<i64>,
        message: String,
    },
    #[error("turn_failed reason={0}")]
    TurnFailed(String),
    #[error("turn_cancelled reason={0}")]
    TurnCancelled(String),
    #[error("turn_input_required")]
    TurnInputRequired,
    #[error("startup_failed reason={0}")]
    StartupFailed(String),
    #[error("io reason={0}")]
    Io(String),
    #[error("malformed reason={0}")]
    Malformed(String),
    #[error("cancelled")]
    Cancelled,
}

impl From<WorkspaceError> for AgentError {
    fn from(value: WorkspaceError) -> Self {
        AgentError::InvalidWorkspaceCwd(value.to_string())
    }
}

pub struct CodexSession {
    issue_id: String,
    issue_identifier: String,
    config: ServiceConfig,
    workspace_path: PathBuf,
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    pid: Option<String>,
    next_id: u64,
    thread_id: String,
    update_tx: mpsc::UnboundedSender<AgentUpdate>,
}

#[derive(Debug)]
enum HandleOutcome {
    None,
    Response {
        id: u64,
        result: Value,
    },
    TurnCompleted {
        turn_id: Option<String>,
        status: String,
        error: Option<String>,
    },
}

impl CodexSession {
    pub async fn start(
        issue_id: String,
        issue_identifier: String,
        config: ServiceConfig,
        workspace_path: PathBuf,
        update_tx: mpsc::UnboundedSender<AgentUpdate>,
        cancel: &CancellationToken,
    ) -> Result<Self, AgentError> {
        validate_agent_cwd(&workspace_path, &workspace_path)?;

        let mut child = Command::new("bash")
            .arg("-lc")
            .arg(&config.codex.command)
            .current_dir(&workspace_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| AgentError::CodexNotFound(err.to_string()))?;

        let pid = child.id().map(|pid| pid.to_string());
        if let Some(stderr) = child.stderr.take() {
            let issue_identifier_for_log = issue_identifier.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    warn!(issue_identifier = issue_identifier_for_log, stream = "stderr", message = %line, "codex_app_server diagnostic");
                }
            });
        }

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AgentError::StartupFailed("missing stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AgentError::StartupFailed("missing stdout".to_string()))?;

        let mut session = Self {
            issue_id,
            issue_identifier,
            config,
            workspace_path,
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            pid,
            next_id: 1,
            thread_id: String::new(),
            update_tx,
        };

        session.initialize(cancel).await?;
        let thread_id = session.start_thread(cancel).await?;
        session.thread_id = thread_id.clone();
        session.emit("session_started", Some(thread_id), None, None, None, None);
        Ok(session)
    }

    pub async fn run_turn(
        &mut self,
        input: String,
        cancel: &CancellationToken,
    ) -> Result<String, AgentError> {
        let turn_response = self
            .request(
                "turn/start",
                self.turn_start_params(input),
                self.config.codex.read_timeout_ms,
                cancel,
            )
            .await?;

        let turn_id = turn_response
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                AgentError::Malformed("turn/start response missing turn.id".to_string())
            })?;

        self.emit(
            "turn_started",
            Some(self.thread_id.clone()),
            Some(turn_id.clone()),
            None,
            None,
            Some(turn_response),
        );

        let completion = timeout(
            Duration::from_millis(self.config.codex.turn_timeout_ms),
            async {
                loop {
                    match self.next_handled_message(cancel).await? {
                        HandleOutcome::TurnCompleted {
                            turn_id: completed_turn_id,
                            status,
                            error,
                        } if completed_turn_id.as_deref() == Some(turn_id.as_str()) => {
                            return Ok::<_, AgentError>((status, error));
                        }
                        _ => {}
                    }
                }
            },
        )
        .await
        .map_err(|_| AgentError::TurnTimeout)??;

        match completion {
            (status, _) if status == "completed" => Ok(turn_id),
            (status, error) if status == "interrupted" => Err(AgentError::TurnCancelled(
                error.unwrap_or_else(|| "turn interrupted".to_string()),
            )),
            (status, error) => Err(AgentError::TurnFailed(
                error.unwrap_or_else(|| format!("turn status {status}")),
            )),
        }
    }

    pub async fn shutdown(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill().await;
        }
    }

    async fn initialize(&mut self, cancel: &CancellationToken) -> Result<(), AgentError> {
        self.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "symphony-rust",
                    "title": "Symphony",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": {
                    "experimentalApi": true,
                }
            }),
            self.config.codex.read_timeout_ms,
            cancel,
        )
        .await
        .map(|_| ())
    }

    async fn start_thread(&mut self, cancel: &CancellationToken) -> Result<String, AgentError> {
        let mut params = json!({
            "cwd": self.workspace_path.to_string_lossy(),
            "ephemeral": true,
            "serviceName": "symphony",
            "sessionStartSource": "startup",
        });
        if let Some(value) = self.config.codex.approval_policy.clone() {
            params["approvalPolicy"] = value;
        }
        if let Some(value) = self.config.codex.thread_sandbox.clone() {
            params["sandbox"] = value;
        }

        let response = self
            .request(
                "thread/start",
                params,
                self.config.codex.read_timeout_ms,
                cancel,
            )
            .await?;

        response
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                AgentError::Malformed("thread/start response missing thread.id".to_string())
            })
    }

    fn turn_start_params(&self, input: String) -> Value {
        let mut params = json!({
            "threadId": self.thread_id,
            "cwd": self.workspace_path.to_string_lossy(),
            "input": [{ "type": "text", "text": input }],
        });
        if let Some(value) = self.config.codex.approval_policy.clone() {
            params["approvalPolicy"] = value;
        }
        if let Some(value) = self.config.codex.turn_sandbox_policy.clone() {
            params["sandboxPolicy"] = value;
        }
        params
    }

    async fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout_ms: u64,
        cancel: &CancellationToken,
    ) -> Result<Value, AgentError> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_json(&json!({ "id": id, "method": method, "params": params }))
            .await?;

        timeout(Duration::from_millis(timeout_ms), async {
            loop {
                match self.next_handled_message(cancel).await? {
                    HandleOutcome::Response {
                        id: response_id,
                        result,
                    } if response_id == id => {
                        return Ok(result);
                    }
                    _ => {}
                }
            }
        })
        .await
        .map_err(|_| AgentError::ResponseTimeout(method.to_string()))?
    }

    async fn next_handled_message(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<HandleOutcome, AgentError> {
        let message = self.read_message(cancel).await?;
        self.handle_message(message).await
    }

    async fn read_message(&mut self, cancel: &CancellationToken) -> Result<Value, AgentError> {
        loop {
            let line = tokio::select! {
                _ = cancel.cancelled() => return Err(AgentError::Cancelled),
                line = self.stdout.next_line() => line.map_err(|err| AgentError::Io(err.to_string()))?,
            };

            match line {
                Some(line) if line.trim().is_empty() => continue,
                Some(line) => {
                    debug!(line = %line, "codex_app_server recv");
                    match serde_json::from_str::<Value>(&line) {
                        Ok(value) => return Ok(value),
                        Err(err) => {
                            self.emit(
                                "malformed",
                                None,
                                None,
                                None,
                                None,
                                Some(json!({ "line": line, "error": err.to_string() })),
                            );
                            return Err(AgentError::Malformed(err.to_string()));
                        }
                    }
                }
                None => return Err(AgentError::PortExit),
            }
        }
    }

    async fn handle_message(&mut self, message: Value) -> Result<HandleOutcome, AgentError> {
        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            if let Some(error) = message.get("error") {
                let code = error.get("code").and_then(Value::as_i64);
                let text = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown response error")
                    .to_string();
                return Err(AgentError::ResponseError {
                    method: "response".to_string(),
                    code,
                    message: text,
                });
            }
            if let Some(result) = message.get("result") {
                return Ok(HandleOutcome::Response {
                    id,
                    result: result.clone(),
                });
            }
        }

        if message.get("id").is_some() && message.get("method").is_some() {
            return self.handle_server_request(message).await;
        }

        if let Some(method) = message.get("method").and_then(Value::as_str) {
            let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
            return self.handle_notification(method, params);
        }

        Ok(HandleOutcome::None)
    }

    async fn handle_server_request(&mut self, message: Value) -> Result<HandleOutcome, AgentError> {
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));

        match method {
            "item/commandExecution/requestApproval" => {
                self.emit(
                    "approval_auto_approved",
                    None,
                    None,
                    None,
                    None,
                    Some(params),
                );
                self.write_json(&json!({ "id": id, "result": { "decision": "acceptForSession" } }))
                    .await?;
            }
            "item/fileChange/requestApproval" => {
                self.emit(
                    "approval_auto_approved",
                    None,
                    None,
                    None,
                    None,
                    Some(params),
                );
                self.write_json(&json!({ "id": id, "result": { "decision": "acceptForSession" } }))
                    .await?;
            }
            "item/tool/requestUserInput" | "mcpServer/elicitation/request" => {
                self.emit("turn_input_required", None, None, None, None, Some(params));
                self.write_json(&json!({
                    "id": id,
                    "error": { "code": -32000, "message": "Symphony does not provide interactive user input" }
                }))
                .await?;
                return Err(AgentError::TurnInputRequired);
            }
            "item/tool/call" => {
                let result = self.handle_dynamic_tool_call(params).await;
                self.write_json(&json!({ "id": id, "result": result }))
                    .await?;
            }
            _ => {
                self.emit(
                    "unsupported_tool_call",
                    None,
                    None,
                    None,
                    None,
                    Some(message.clone()),
                );
                self.write_json(&json!({
                    "id": id,
                    "error": { "code": -32601, "message": format!("Unsupported Symphony client request: {method}") }
                }))
                .await?;
            }
        }

        Ok(HandleOutcome::None)
    }

    async fn handle_dynamic_tool_call(&self, params: Value) -> Value {
        let tool = params
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if tool != "linear_graphql" {
            return dynamic_tool_result(
                false,
                json!({ "error": format!("unsupported tool: {tool}") }),
            );
        }

        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let (query, variables) = match parse_linear_graphql_arguments(arguments) {
            Ok(parsed) => parsed,
            Err(error) => return dynamic_tool_result(false, json!({ "error": error })),
        };

        match linear_graphql_tool(&self.config, &query, variables).await {
            Ok(payload) => dynamic_tool_result(true, payload),
            Err(error) => dynamic_tool_result(false, json!({ "error": error.to_string() })),
        }
    }

    fn handle_notification(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<HandleOutcome, AgentError> {
        let thread_id = extract_thread_id(&params)
            .or_else(|| (!self.thread_id.is_empty()).then(|| self.thread_id.clone()));
        let turn_id = extract_turn_id(&params);
        let usage = extract_token_usage(method, &params);
        let rate_limits = extract_rate_limits(&params);
        let event = event_name(method, &params);
        self.emit(
            &event,
            thread_id.clone(),
            turn_id.clone(),
            usage,
            rate_limits,
            Some(params.clone()),
        );

        if method == "turn/completed" {
            let status = params
                .pointer("/turn/status")
                .and_then(Value::as_str)
                .unwrap_or("failed")
                .to_string();
            let error = params
                .pointer("/turn/error/message")
                .and_then(Value::as_str)
                .map(str::to_string);
            return Ok(HandleOutcome::TurnCompleted {
                turn_id,
                status,
                error,
            });
        }

        if method == "error" {
            return Err(AgentError::TurnFailed(
                params
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("codex error notification")
                    .to_string(),
            ));
        }

        Ok(HandleOutcome::None)
    }

    async fn write_json(&mut self, value: &Value) -> Result<(), AgentError> {
        let mut line = serde_json::to_vec(value).map_err(|err| AgentError::Io(err.to_string()))?;
        line.push(b'\n');
        debug!(message = %String::from_utf8_lossy(&line), "codex_app_server send");
        self.stdin
            .write_all(&line)
            .await
            .map_err(|err| AgentError::Io(err.to_string()))?;
        self.stdin
            .flush()
            .await
            .map_err(|err| AgentError::Io(err.to_string()))
    }

    fn emit(
        &self,
        event: &str,
        thread_id: Option<String>,
        turn_id: Option<String>,
        usage: Option<TokenUsage>,
        rate_limits: Option<Value>,
        message: Option<Value>,
    ) {
        let _ = self.update_tx.send(AgentUpdate {
            issue_id: self.issue_id.clone(),
            issue_identifier: self.issue_identifier.clone(),
            event: event.to_string(),
            timestamp: Utc::now(),
            codex_app_server_pid: self.pid.clone(),
            thread_id,
            turn_id,
            usage,
            rate_limits,
            message,
        });
    }
}

fn parse_linear_graphql_arguments(arguments: Value) -> Result<(String, Option<Value>), String> {
    if let Some(query) = arguments.as_str() {
        return Ok((query.to_string(), None));
    }

    let query = arguments
        .get("query")
        .and_then(Value::as_str)
        .filter(|query| !query.trim().is_empty())
        .ok_or_else(|| "linear_graphql requires a non-empty query string".to_string())?
        .to_string();
    let variables = arguments.get("variables").cloned();
    if variables.as_ref().is_some_and(|value| !value.is_object()) {
        return Err("linear_graphql variables must be a JSON object".to_string());
    }
    Ok((query, variables))
}

fn dynamic_tool_result(success: bool, payload: Value) -> Value {
    let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    json!({
        "success": success,
        "contentItems": [{ "type": "inputText", "text": text }]
    })
}

fn event_name(method: &str, params: &Value) -> String {
    match method {
        "thread/started" => "session_started".to_string(),
        "turn/started" => "turn_started".to_string(),
        "thread/tokenUsage/updated" => "token_usage_updated".to_string(),
        "turn/completed" => match params.pointer("/turn/status").and_then(Value::as_str) {
            Some("completed") => "turn_completed".to_string(),
            Some("interrupted") => "turn_cancelled".to_string(),
            Some("failed") => "turn_failed".to_string(),
            _ => "turn_ended_with_error".to_string(),
        },
        "error" => "turn_ended_with_error".to_string(),
        _ => "notification".to_string(),
    }
}

fn extract_thread_id(params: &Value) -> Option<String> {
    params
        .get("threadId")
        .or_else(|| params.pointer("/thread/id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn extract_turn_id(params: &Value) -> Option<String> {
    params
        .get("turnId")
        .or_else(|| params.pointer("/turn/id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn extract_token_usage(method: &str, params: &Value) -> Option<TokenUsage> {
    let absolute_payload = if method == "thread/tokenUsage/updated" {
        Some(params)
    } else {
        params
            .get("total_token_usage")
            .or_else(|| params.get("totalTokenUsage"))
    }?;

    let input = find_number(
        absolute_payload,
        &[
            "input_tokens",
            "inputTokens",
            "prompt_tokens",
            "promptTokens",
        ],
    )?;
    let output = find_number(
        absolute_payload,
        &[
            "output_tokens",
            "outputTokens",
            "completion_tokens",
            "completionTokens",
        ],
    )?;
    let total =
        find_number(absolute_payload, &["total_tokens", "totalTokens"]).unwrap_or(input + output);

    Some(TokenUsage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: total,
        absolute: true,
    })
}

fn find_number(value: &Value, keys: &[&str]) -> Option<i64> {
    match value {
        Value::Object(map) => {
            for key in keys {
                if let Some(number) = map.get(*key).and_then(Value::as_i64) {
                    return Some(number);
                }
                if let Some(number) = map.get(*key).and_then(Value::as_u64) {
                    return Some(number as i64);
                }
            }
            map.values().find_map(|value| find_number(value, keys))
        }
        Value::Array(values) => values.iter().find_map(|value| find_number(value, keys)),
        _ => None,
    }
}

fn extract_rate_limits(params: &Value) -> Option<Value> {
    if let Some(value) = params
        .get("rate_limits")
        .or_else(|| params.get("rateLimits"))
    {
        return Some(value.clone());
    }
    match params {
        Value::Object(map) => map.values().find_map(extract_rate_limits),
        Value::Array(values) => values.iter().find_map(extract_rate_limits),
        _ => None,
    }
}

pub fn continuation_prompt() -> String {
    "Continue working on the issue. Re-check the tracker state and follow WORKFLOW.md handoff instructions before stopping.".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_linear_graphql_shorthand() {
        let (query, variables) =
            parse_linear_graphql_arguments(json!("query { viewer { id } }")).unwrap();

        assert_eq!(query, "query { viewer { id } }");
        assert!(variables.is_none());
    }

    #[test]
    fn extracts_absolute_token_usage() {
        let usage = extract_token_usage(
            "thread/tokenUsage/updated",
            &json!({ "usage": { "inputTokens": 10, "outputTokens": 5, "totalTokens": 15 } }),
        )
        .unwrap();

        assert_eq!(usage.total_tokens, 15);
        assert!(usage.absolute);
    }

    #[test]
    fn validates_cwd_before_launch() {
        let path = std::path::Path::new("/tmp/example");
        assert!(validate_agent_cwd(path, path).is_ok());
    }
}
