use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use thiserror::Error;
use tokio::fs;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{info, warn};

use crate::config::{HooksConfig, normalize_components};

#[derive(Clone, Debug)]
pub struct WorkspaceManager {
    root: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Workspace {
    pub path: PathBuf,
    pub workspace_key: String,
    pub created_now: bool,
}

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("workspace_path_outside_root path={path} root={root}")]
    OutsideRoot { path: PathBuf, root: PathBuf },
    #[error("workspace_path_not_directory path={0}")]
    NotDirectory(PathBuf),
    #[error("workspace_io path={path} reason={reason}")]
    Io { path: PathBuf, reason: String },
    #[error("hook_failed hook={hook} status={status}")]
    HookFailed { hook: &'static str, status: i32 },
    #[error("hook_timeout hook={0}")]
    HookTimeout(&'static str),
    #[error("invalid_workspace_cwd cwd={cwd} workspace_path={workspace_path}")]
    InvalidCwd {
        cwd: PathBuf,
        workspace_path: PathBuf,
    },
}

impl WorkspaceManager {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root: normalize_components(&root),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn workspace_path(
        &self,
        issue_identifier: &str,
    ) -> Result<(String, PathBuf), WorkspaceError> {
        let key = sanitize_workspace_key(issue_identifier);
        let path = normalize_components(&self.root.join(&key));
        ensure_inside_root(&self.root, &path)?;
        Ok((key, path))
    }

    pub async fn ensure_workspace(
        &self,
        issue_identifier: &str,
        hooks: &HooksConfig,
    ) -> Result<Workspace, WorkspaceError> {
        fs::create_dir_all(&self.root)
            .await
            .map_err(|err| WorkspaceError::Io {
                path: self.root.clone(),
                reason: err.to_string(),
            })?;

        let (workspace_key, path) = self.workspace_path(issue_identifier)?;
        let exists = fs::try_exists(&path)
            .await
            .map_err(|err| WorkspaceError::Io {
                path: path.clone(),
                reason: err.to_string(),
            })?;

        if exists {
            let metadata = fs::metadata(&path)
                .await
                .map_err(|err| WorkspaceError::Io {
                    path: path.clone(),
                    reason: err.to_string(),
                })?;
            if !metadata.is_dir() {
                return Err(WorkspaceError::NotDirectory(path));
            }
            return Ok(Workspace {
                path,
                workspace_key,
                created_now: false,
            });
        }

        fs::create_dir(&path)
            .await
            .map_err(|err| WorkspaceError::Io {
                path: path.clone(),
                reason: err.to_string(),
            })?;

        if let Some(script) = hooks.after_create.as_deref() {
            run_hook("after_create", script, &path, hooks.timeout_ms).await?;
        }

        Ok(Workspace {
            path,
            workspace_key,
            created_now: true,
        })
    }

    pub async fn remove_workspace(
        &self,
        issue_identifier: &str,
        hooks: &HooksConfig,
    ) -> Result<(), WorkspaceError> {
        let (_, path) = self.workspace_path(issue_identifier)?;
        if !fs::try_exists(&path)
            .await
            .map_err(|err| WorkspaceError::Io {
                path: path.clone(),
                reason: err.to_string(),
            })?
        {
            return Ok(());
        }

        if let Some(script) = hooks.before_remove.as_deref()
            && let Err(err) = run_hook("before_remove", script, &path, hooks.timeout_ms).await
        {
            warn!(error = %err, path = %path.display(), "workspace_before_remove_hook failed_ignored");
        }

        fs::remove_dir_all(&path)
            .await
            .map_err(|err| WorkspaceError::Io {
                path,
                reason: err.to_string(),
            })
    }
}

pub async fn run_before_run(path: &Path, hooks: &HooksConfig) -> Result<(), WorkspaceError> {
    if let Some(script) = hooks.before_run.as_deref() {
        run_hook("before_run", script, path, hooks.timeout_ms).await?;
    }
    Ok(())
}

pub async fn run_after_run(path: &Path, hooks: &HooksConfig) {
    if let Some(script) = hooks.after_run.as_deref()
        && let Err(err) = run_hook("after_run", script, path, hooks.timeout_ms).await
    {
        warn!(error = %err, path = %path.display(), "workspace_after_run_hook failed_ignored");
    }
}

async fn run_hook(
    hook: &'static str,
    script: &str,
    cwd: &Path,
    timeout_ms: u64,
) -> Result<(), WorkspaceError> {
    info!(hook, cwd = %cwd.display(), "workspace_hook starting");
    let mut child = Command::new("sh")
        .arg("-lc")
        .arg(script)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|err| WorkspaceError::Io {
            path: cwd.to_path_buf(),
            reason: err.to_string(),
        })?;

    let status = timeout(Duration::from_millis(timeout_ms), child.wait())
        .await
        .map_err(|_| WorkspaceError::HookTimeout(hook))?
        .map_err(|err| WorkspaceError::Io {
            path: cwd.to_path_buf(),
            reason: err.to_string(),
        })?;

    if status.success() {
        info!(hook, cwd = %cwd.display(), "workspace_hook completed");
        Ok(())
    } else {
        Err(WorkspaceError::HookFailed {
            hook,
            status: status.code().unwrap_or(-1),
        })
    }
}

pub fn sanitize_workspace_key(identifier: &str) -> String {
    let sanitized = identifier
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();

    if sanitized.is_empty() {
        "_".to_string()
    } else {
        sanitized
    }
}

pub fn ensure_inside_root(root: &Path, path: &Path) -> Result<(), WorkspaceError> {
    let root = normalize_components(root);
    let path = normalize_components(path);
    if path.starts_with(&root) {
        Ok(())
    } else {
        Err(WorkspaceError::OutsideRoot { path, root })
    }
}

pub fn validate_agent_cwd(cwd: &Path, workspace_path: &Path) -> Result<(), WorkspaceError> {
    let cwd = normalize_components(cwd);
    let workspace_path = normalize_components(workspace_path);
    if cwd == workspace_path {
        Ok(())
    } else {
        Err(WorkspaceError::InvalidCwd {
            cwd,
            workspace_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_workspace_key() {
        assert_eq!(sanitize_workspace_key("ABC-1 bad/name"), "ABC-1_bad_name");
    }

    #[tokio::test]
    async fn creates_and_reuses_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let manager = WorkspaceManager::new(dir.path().to_path_buf());
        let hooks = HooksConfig {
            timeout_ms: 1_000,
            ..HooksConfig::default()
        };

        let first = manager.ensure_workspace("ABC-1", &hooks).await.unwrap();
        let second = manager.ensure_workspace("ABC-1", &hooks).await.unwrap();

        assert!(first.created_now);
        assert!(!second.created_now);
        assert_eq!(first.path, second.path);
    }
}
