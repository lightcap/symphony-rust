use std::collections::{HashMap, HashSet};
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
    let original_env = env::vars().map(|(key, _)| key).collect::<HashSet<_>>();
    let paths = dotenv_paths(&cwd, workflow_path);
    let (loaded, updates) = collect_dotenv_updates(paths, &original_env)?;

    for (key, value) in updates {
        set_env_var(&key, &value);
    }

    Ok(loaded)
}

fn dotenv_paths(cwd: &Path, workflow_path: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    push_unique_dir(&mut dirs, cwd, cwd.to_path_buf());

    let workflow_dir = workflow_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let workflow_dir = if workflow_dir.is_absolute() {
        workflow_dir.to_path_buf()
    } else {
        cwd.join(workflow_dir)
    };
    push_unique_dir(&mut dirs, cwd, workflow_dir);

    dirs.into_iter()
        .flat_map(|dir| DOTENV_FILES.map(move |file_name| dir.join(file_name)))
        .collect()
}

fn collect_dotenv_updates(
    paths: Vec<PathBuf>,
    original_env: &HashSet<String>,
) -> Result<(Vec<PathBuf>, HashMap<String, String>), EnvLoadError> {
    let mut loaded = Vec::new();
    let mut updates = HashMap::new();

    for path in paths {
        if !path.exists() {
            continue;
        }
        let entries = dotenvy::from_path_iter(&path).map_err(|err| EnvLoadError::Parse {
            path: path.clone(),
            reason: err.to_string(),
        })?;
        for entry in entries {
            let (key, value) = entry.map_err(|err| EnvLoadError::Parse {
                path: path.clone(),
                reason: err.to_string(),
            })?;
            if !original_env.contains(&key) {
                updates.insert(key, value);
            }
        }
        loaded.push(path);
    }

    Ok((loaded, updates))
}

fn set_env_var(key: &str, value: &str) {
    // SAFETY: Symphony loads dotenv files during single-threaded startup before worker tasks or
    // subprocesses are spawned. No concurrent environment readers/writers are active here.
    unsafe { env::set_var(key, value) }
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
    use std::fs;

    #[test]
    fn deduplicates_default_workflow_dir() {
        let cwd = PathBuf::from("/tmp/example");
        let mut dirs = Vec::new();

        push_unique_dir(&mut dirs, &cwd, cwd.clone());
        push_unique_dir(&mut dirs, &cwd, PathBuf::from("."));

        assert_eq!(dirs, vec![cwd]);
    }

    #[test]
    fn later_dotenv_files_override_earlier_dotenv_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".env"), "LINEAR_API_KEY=\nKEEP=from_env\n").unwrap();
        fs::write(
            dir.path().join(".env.local"),
            "LINEAR_API_KEY=real-token\nKEEP=from_file\n",
        )
        .unwrap();
        let original_env = HashSet::from(["KEEP".to_string()]);

        let (loaded, updates) = collect_dotenv_updates(
            vec![dir.path().join(".env"), dir.path().join(".env.local")],
            &original_env,
        )
        .unwrap();

        assert_eq!(loaded.len(), 2);
        assert_eq!(
            updates.get("LINEAR_API_KEY"),
            Some(&"real-token".to_string())
        );
        assert!(!updates.contains_key("KEEP"));
    }

    #[test]
    fn workflow_directory_dotenv_overrides_current_directory_dotenv() {
        let cwd = tempfile::tempdir().unwrap();
        let workflow_dir = cwd.path().join("workflow");
        fs::create_dir(&workflow_dir).unwrap();
        fs::write(cwd.path().join(".env"), "LINEAR_API_KEY=cwd-token\n").unwrap();
        fs::write(workflow_dir.join(".env"), "LINEAR_API_KEY=workflow-token\n").unwrap();

        let paths = dotenv_paths(cwd.path(), &workflow_dir.join("WORKFLOW.md"));
        let (_, updates) = collect_dotenv_updates(paths, &HashSet::new()).unwrap();

        assert_eq!(
            updates.get("LINEAR_API_KEY"),
            Some(&"workflow-token".to_string())
        );
    }
}
