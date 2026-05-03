use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use serde_yaml::{Mapping, Value};
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::warn;

use crate::workflow::{self, WorkflowDefinition, WorkflowError};

const DEFAULT_LINEAR_ENDPOINT: &str = "https://api.linear.app/graphql";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackerConfig {
    pub kind: Option<String>,
    pub endpoint: String,
    pub api_key: Option<String>,
    pub project_slug: Option<String>,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PollingConfig {
    pub interval_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    pub root: PathBuf,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HooksConfig {
    pub after_create: Option<String>,
    pub before_run: Option<String>,
    pub after_run: Option<String>,
    pub before_remove: Option<String>,
    pub timeout_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentConfig {
    pub max_concurrent_agents: usize,
    pub max_turns: u32,
    pub max_retry_backoff_ms: u64,
    pub max_concurrent_agents_by_state: HashMap<String, usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CodexConfig {
    pub command: String,
    pub approval_policy: Option<JsonValue>,
    pub thread_sandbox: Option<JsonValue>,
    pub turn_sandbox_policy: Option<JsonValue>,
    pub turn_timeout_ms: u64,
    pub read_timeout_ms: u64,
    pub stall_timeout_ms: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ServerConfig {
    pub port: Option<u16>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub workflow_path: PathBuf,
    pub workflow_dir: PathBuf,
    pub workflow_modified: Option<SystemTime>,
    pub prompt_template: String,
    pub tracker: TrackerConfig,
    pub polling: PollingConfig,
    pub workspace: WorkspaceConfig,
    pub hooks: HooksConfig,
    pub agent: AgentConfig,
    pub codex: CodexConfig,
    pub server: ServerConfig,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error(transparent)]
    Workflow(#[from] WorkflowError),
    #[error("invalid_config field={field} reason={reason}")]
    Invalid { field: &'static str, reason: String },
}

#[derive(Debug)]
struct ConfigState {
    current: ServiceConfig,
    last_error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ConfigManager {
    workflow_path: PathBuf,
    state: Arc<RwLock<ConfigState>>,
}

impl ConfigManager {
    pub fn workflow_path_from_cli(path: Option<PathBuf>) -> PathBuf {
        path.unwrap_or_else(|| PathBuf::from("WORKFLOW.md"))
    }

    pub fn load_initial(path: PathBuf) -> Result<Self, ConfigError> {
        let config = load_service_config(path)?;
        config.validate_for_dispatch()?;
        let workflow_path = config.workflow_path.clone();

        Ok(Self {
            workflow_path,
            state: Arc::new(RwLock::new(ConfigState {
                current: config,
                last_error: None,
            })),
        })
    }

    pub async fn current(&self) -> ServiceConfig {
        self.state.read().await.current.clone()
    }

    pub async fn last_error(&self) -> Option<String> {
        self.state.read().await.last_error.clone()
    }

    pub async fn refresh_for_dispatch(&self) -> Result<ServiceConfig, ConfigError> {
        match load_service_config(self.workflow_path.clone()).and_then(|config| {
            config.validate_for_dispatch()?;
            Ok(config)
        }) {
            Ok(config) => {
                let mut state = self.state.write().await;
                state.current = config.clone();
                state.last_error = None;
                Ok(config)
            }
            Err(err) => {
                let message = err.to_string();
                let mut state = self.state.write().await;
                state.last_error = Some(message.clone());
                warn!(error = %message, "workflow_reload failed");
                Err(err)
            }
        }
    }
}

pub fn load_service_config(path: PathBuf) -> Result<ServiceConfig, ConfigError> {
    let workflow_path = absolutize_existing_or_selected(path)?;
    let workflow_dir = workflow_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let modified = fs::metadata(&workflow_path)
        .and_then(|meta| meta.modified())
        .ok();
    let workflow = workflow::load_workflow(&workflow_path)?;

    ServiceConfig::from_workflow(workflow_path, workflow_dir, modified, workflow)
}

impl ServiceConfig {
    pub fn from_workflow(
        workflow_path: PathBuf,
        workflow_dir: PathBuf,
        workflow_modified: Option<SystemTime>,
        workflow: WorkflowDefinition,
    ) -> Result<Self, ConfigError> {
        let root = &workflow.config;
        let tracker_map = object(root, "tracker");
        let polling_map = object(root, "polling");
        let workspace_map = object(root, "workspace");
        let hooks_map = object(root, "hooks");
        let agent_map = object(root, "agent");
        let codex_map = object(root, "codex");
        let server_map = object(root, "server");

        let tracker_kind = string(tracker_map, "kind");
        let endpoint =
            string(tracker_map, "endpoint").unwrap_or_else(|| DEFAULT_LINEAR_ENDPOINT.to_string());
        let api_key = resolve_secret(string(tracker_map, "api_key"));
        let project_slug = string(tracker_map, "project_slug");
        let active_states = string_list(tracker_map, "active_states")
            .unwrap_or_else(|| vec!["Todo".to_string(), "In Progress".to_string()]);
        let terminal_states = string_list(tracker_map, "terminal_states").unwrap_or_else(|| {
            vec![
                "Closed".to_string(),
                "Cancelled".to_string(),
                "Canceled".to_string(),
                "Duplicate".to_string(),
                "Done".to_string(),
            ]
        });

        let workspace_root = match string(workspace_map, "root") {
            Some(value) => resolve_path(&value, &workflow_dir)?,
            None => normalize_absolute(env::temp_dir().join("symphony_workspaces")),
        };

        let hooks_timeout_ms = integer(hooks_map, "timeout_ms")
            .unwrap_or(60_000)
            .try_into()
            .map_err(|_| ConfigError::Invalid {
                field: "hooks.timeout_ms",
                reason: "must be a non-negative integer".to_string(),
            })?;
        if hooks_timeout_ms == 0 {
            return Err(ConfigError::Invalid {
                field: "hooks.timeout_ms",
                reason: "must be positive".to_string(),
            });
        }

        let max_concurrent_agents = positive_usize(agent_map, "max_concurrent_agents", 10)?;
        let max_turns = positive_u32(agent_map, "max_turns", 20)?;
        let max_retry_backoff_ms = positive_u64(agent_map, "max_retry_backoff_ms", 300_000)?;
        let max_concurrent_agents_by_state = positive_state_limits(agent_map);

        let codex_command =
            string(codex_map, "command").unwrap_or_else(|| "codex app-server".to_string());
        let codex_command = codex_command.trim().to_string();
        let turn_timeout_ms = positive_u64(codex_map, "turn_timeout_ms", 3_600_000)?;
        let read_timeout_ms = positive_u64(codex_map, "read_timeout_ms", 5_000)?;
        let stall_timeout_ms = integer(codex_map, "stall_timeout_ms").unwrap_or(300_000);

        let server_port = integer(server_map, "port")
            .map(|port| {
                if (0..=u16::MAX as i64).contains(&port) {
                    Ok(port as u16)
                } else {
                    Err(ConfigError::Invalid {
                        field: "server.port",
                        reason: "must be between 0 and 65535".to_string(),
                    })
                }
            })
            .transpose()?;

        Ok(Self {
            workflow_path,
            workflow_dir,
            workflow_modified,
            prompt_template: workflow.prompt_template,
            tracker: TrackerConfig {
                kind: tracker_kind,
                endpoint,
                api_key,
                project_slug,
                active_states,
                terminal_states,
            },
            polling: PollingConfig {
                interval_ms: positive_u64(polling_map, "interval_ms", 30_000)?,
            },
            workspace: WorkspaceConfig {
                root: workspace_root,
            },
            hooks: HooksConfig {
                after_create: string(hooks_map, "after_create"),
                before_run: string(hooks_map, "before_run"),
                after_run: string(hooks_map, "after_run"),
                before_remove: string(hooks_map, "before_remove"),
                timeout_ms: hooks_timeout_ms,
            },
            agent: AgentConfig {
                max_concurrent_agents,
                max_turns,
                max_retry_backoff_ms,
                max_concurrent_agents_by_state,
            },
            codex: CodexConfig {
                command: codex_command,
                approval_policy: json_value(codex_map, "approval_policy"),
                thread_sandbox: json_value(codex_map, "thread_sandbox"),
                turn_sandbox_policy: json_value(codex_map, "turn_sandbox_policy"),
                turn_timeout_ms,
                read_timeout_ms,
                stall_timeout_ms,
            },
            server: ServerConfig { port: server_port },
        })
    }

    pub fn validate_for_dispatch(&self) -> Result<(), ConfigError> {
        let kind = self.tracker.kind.as_deref().unwrap_or_default();
        if kind != "linear" {
            return Err(ConfigError::Invalid {
                field: "tracker.kind",
                reason: "supported value is linear".to_string(),
            });
        }

        if self
            .tracker
            .api_key
            .as_deref()
            .unwrap_or_default()
            .trim()
            .is_empty()
        {
            return Err(ConfigError::Invalid {
                field: "tracker.api_key",
                reason: "missing after environment resolution".to_string(),
            });
        }

        if self
            .tracker
            .project_slug
            .as_deref()
            .unwrap_or_default()
            .trim()
            .is_empty()
        {
            return Err(ConfigError::Invalid {
                field: "tracker.project_slug",
                reason: "required for linear tracker".to_string(),
            });
        }

        if self.codex.command.trim().is_empty() {
            return Err(ConfigError::Invalid {
                field: "codex.command",
                reason: "must be non-empty".to_string(),
            });
        }

        Ok(())
    }
}

fn object<'a>(map: &'a Mapping, key: &'static str) -> Option<&'a Mapping> {
    map.get(Value::String(key.to_string()))
        .and_then(Value::as_mapping)
}

fn get<'a>(map: Option<&'a Mapping>, key: &'static str) -> Option<&'a Value> {
    map.and_then(|map| map.get(Value::String(key.to_string())))
}

fn string(map: Option<&Mapping>, key: &'static str) -> Option<String> {
    get(map, key)
        .and_then(Value::as_str)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn integer(map: Option<&Mapping>, key: &'static str) -> Option<i64> {
    get(map, key).and_then(Value::as_i64)
}

fn string_list(map: Option<&Mapping>, key: &'static str) -> Option<Vec<String>> {
    let values = get(map, key)?.as_sequence()?;
    let strings = values
        .iter()
        .filter_map(Value::as_str)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    Some(strings)
}

fn json_value(map: Option<&Mapping>, key: &'static str) -> Option<JsonValue> {
    get(map, key).and_then(|value| serde_json::to_value(value).ok())
}

fn positive_usize(
    map: Option<&Mapping>,
    key: &'static str,
    default: usize,
) -> Result<usize, ConfigError> {
    match integer(map, key) {
        Some(value) if value > 0 => Ok(value as usize),
        Some(_) => Err(ConfigError::Invalid {
            field: key,
            reason: "must be positive".to_string(),
        }),
        None => Ok(default),
    }
}

fn positive_u32(
    map: Option<&Mapping>,
    key: &'static str,
    default: u32,
) -> Result<u32, ConfigError> {
    match integer(map, key) {
        Some(value) if value > 0 && value <= u32::MAX as i64 => Ok(value as u32),
        Some(_) => Err(ConfigError::Invalid {
            field: key,
            reason: "must be a positive u32".to_string(),
        }),
        None => Ok(default),
    }
}

fn positive_u64(
    map: Option<&Mapping>,
    key: &'static str,
    default: u64,
) -> Result<u64, ConfigError> {
    match integer(map, key) {
        Some(value) if value > 0 => Ok(value as u64),
        Some(_) => Err(ConfigError::Invalid {
            field: key,
            reason: "must be positive".to_string(),
        }),
        None => Ok(default),
    }
}

fn positive_state_limits(map: Option<&Mapping>) -> HashMap<String, usize> {
    get(map, "max_concurrent_agents_by_state")
        .and_then(Value::as_mapping)
        .map(|limits| {
            limits
                .iter()
                .filter_map(|(key, value)| {
                    let state = key.as_str()?.to_lowercase();
                    let limit = value.as_i64()?;
                    (limit > 0).then_some((state, limit as usize))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn resolve_secret(value: Option<String>) -> Option<String> {
    let value = value?;
    if let Some(name) = env_name(&value) {
        env::var(name)
            .ok()
            .filter(|resolved| !resolved.trim().is_empty())
    } else {
        Some(value)
    }
}

fn env_name(value: &str) -> Option<&str> {
    let name = value.strip_prefix('$')?;
    (!name.is_empty() && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric()))
        .then_some(name)
}

fn resolve_path(value: &str, base_dir: &Path) -> Result<PathBuf, ConfigError> {
    let expanded = if let Some(name) = env_name(value) {
        env::var(name).map_err(|_| ConfigError::Invalid {
            field: "workspace.root",
            reason: format!("environment variable {name} is not set"),
        })?
    } else if value == "~" || value.starts_with("~/") {
        let home = env::var("HOME").map_err(|_| ConfigError::Invalid {
            field: "workspace.root",
            reason: "HOME is not set for ~ expansion".to_string(),
        })?;
        format!("{home}{}", &value[1..])
    } else {
        value.to_string()
    };

    let path = PathBuf::from(expanded);
    let absolute = if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    };
    Ok(normalize_absolute(absolute))
}

fn normalize_absolute(path: PathBuf) -> PathBuf {
    let absolute = if path.is_absolute() {
        path
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    normalize_components(&absolute)
}

pub fn normalize_components(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

fn absolutize_existing_or_selected(path: PathBuf) -> Result<PathBuf, ConfigError> {
    if path.exists() {
        fs::canonicalize(&path).map_err(|_| WorkflowError::MissingWorkflowFile(path).into())
    } else {
        let absolute = normalize_absolute(path);
        Err(WorkflowError::MissingWorkflowFile(absolute).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workflow(content: &str) -> (WorkflowDefinition, tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("WORKFLOW.md");
        fs::write(&path, content).unwrap();
        let definition = workflow::load_workflow(&path).unwrap();
        (definition, dir, path)
    }

    #[test]
    fn applies_defaults_and_resolves_workspace_relative_to_workflow() {
        let (definition, dir, path) = workflow(
            "---\ntracker:\n  kind: linear\n  api_key: token\n  project_slug: demo\nworkspace:\n  root: .workspaces\n---\nHi",
        );

        let config =
            ServiceConfig::from_workflow(path, dir.path().to_path_buf(), None, definition).unwrap();

        assert_eq!(config.polling.interval_ms, 30_000);
        assert_eq!(config.agent.max_turns, 20);
        assert!(config.workspace.root.ends_with(".workspaces"));
        config.validate_for_dispatch().unwrap();
    }

    #[test]
    fn ignores_invalid_per_state_limits() {
        let (definition, dir, path) = workflow(
            "---\ntracker:\n  kind: linear\n  api_key: token\n  project_slug: demo\nagent:\n  max_concurrent_agents_by_state:\n    Todo: 2\n    Bad: 0\n---\nHi",
        );

        let config =
            ServiceConfig::from_workflow(path, dir.path().to_path_buf(), None, definition).unwrap();

        assert_eq!(
            config.agent.max_concurrent_agents_by_state.get("todo"),
            Some(&2)
        );
        assert!(
            !config
                .agent
                .max_concurrent_agents_by_state
                .contains_key("bad")
        );
    }
}
