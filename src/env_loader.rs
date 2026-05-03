use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::config::normalize_components;

const DOTENV_FILES: [&str; 2] = [".env", ".env.local"];

#[derive(Debug, Error)]
pub enum EnvLoadError {
    #[error("env_current_dir reason={0}")]
    CurrentDir(String),
    #[error("env_file_parse path={path} reason={reason}")]
    Parse { path: PathBuf, reason: String },
}

pub fn load_dotenvs(workflow_path: &Path) -> Result<Vec<PathBuf>, EnvLoadError> {
    let cwd = env::current_dir().map_err(|err| EnvLoadError::CurrentDir(err.to_string()))?;
    let mut dirs = Vec::new();
    push_unique_dir(&mut dirs, &cwd, cwd.clone());

    let workflow_dir = workflow_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let workflow_dir = if workflow_dir.is_absolute() {
        workflow_dir.to_path_buf()
    } else {
        cwd.join(workflow_dir)
    };
    push_unique_dir(&mut dirs, &cwd, workflow_dir);

    let mut loaded = Vec::new();
    for dir in dirs {
        for file_name in DOTENV_FILES {
            let path = dir.join(file_name);
            if !path.exists() {
                continue;
            }
            dotenvy::from_path(&path).map_err(|err| EnvLoadError::Parse {
                path: path.clone(),
                reason: err.to_string(),
            })?;
            loaded.push(path);
        }
    }

    Ok(loaded)
}

fn push_unique_dir(dirs: &mut Vec<PathBuf>, cwd: &Path, path: PathBuf) {
    let absolute = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    let normalized = normalize_components(&absolute);
    let seen = dirs
        .iter()
        .map(|path| normalize_components(path))
        .collect::<HashSet<_>>();
    if !seen.contains(&normalized) {
        dirs.push(normalized);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deduplicates_default_workflow_dir() {
        let cwd = PathBuf::from("/tmp/example");
        let mut dirs = Vec::new();

        push_unique_dir(&mut dirs, &cwd, cwd.clone());
        push_unique_dir(&mut dirs, &cwd, PathBuf::from("."));

        assert_eq!(dirs, vec![cwd]);
    }
}
