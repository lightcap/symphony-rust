use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum EnvLoadError {
    #[error("env_current_dir reason={0}")]
    CurrentDir(String),
    #[error("env_file_load path={path} reason={reason}")]
    Load { path: PathBuf, reason: String },
}

pub fn load_dotenv() -> Result<Option<PathBuf>, EnvLoadError> {
    let cwd = env::current_dir().map_err(|err| EnvLoadError::CurrentDir(err.to_string()))?;
    load_dotenv_path(cwd.join(".env"))
}

fn load_dotenv_path(path: PathBuf) -> Result<Option<PathBuf>, EnvLoadError> {
    match fs::metadata(&path) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(EnvLoadError::Load {
                    path,
                    reason: "path is not a file".to_string(),
                });
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(EnvLoadError::Load {
                path,
                reason: err.to_string(),
            });
        }
    }

    dotenvy::from_path(&path).map_err(|err| EnvLoadError::Load {
        path: path.clone(),
        reason: err.to_string(),
    })?;
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_dotenv_returns_none() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(load_dotenv_path(dir.path().join(".env")).unwrap(), None);
    }
}
