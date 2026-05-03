use std::fs;
use std::path::{Path, PathBuf};

use serde_yaml::{Mapping, Value};
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct WorkflowDefinition {
    pub config: Mapping,
    pub prompt_template: String,
}

#[derive(Debug, Error)]
pub enum WorkflowError {
    #[error("missing_workflow_file path={0}")]
    MissingWorkflowFile(PathBuf),
    #[error("workflow_parse_error path={path} reason={reason}")]
    ParseError { path: PathBuf, reason: String },
    #[error("workflow_front_matter_not_a_map path={0}")]
    FrontMatterNotMap(PathBuf),
}

pub fn load_workflow(path: &Path) -> Result<WorkflowDefinition, WorkflowError> {
    let content = fs::read_to_string(path)
        .map_err(|_| WorkflowError::MissingWorkflowFile(path.to_path_buf()))?;

    let (config, body) = if content.starts_with("---") {
        parse_front_matter(path, &content)?
    } else {
        (Mapping::new(), content)
    };

    Ok(WorkflowDefinition {
        config,
        prompt_template: body.trim().to_string(),
    })
}

fn parse_front_matter(path: &Path, content: &str) -> Result<(Mapping, String), WorkflowError> {
    let mut lines = content.lines();
    let first = lines.next().unwrap_or_default();
    if first.trim_end() != "---" {
        return Ok((Mapping::new(), content.to_string()));
    }

    let mut yaml = String::new();
    let mut body = String::new();
    let mut in_yaml = true;

    for line in lines {
        if in_yaml && line.trim_end() == "---" {
            in_yaml = false;
            continue;
        }

        if in_yaml {
            yaml.push_str(line);
            yaml.push('\n');
        } else {
            body.push_str(line);
            body.push('\n');
        }
    }

    if in_yaml {
        return Err(WorkflowError::ParseError {
            path: path.to_path_buf(),
            reason: "unterminated YAML front matter".to_string(),
        });
    }

    let value: Value = serde_yaml::from_str(&yaml).map_err(|err| WorkflowError::ParseError {
        path: path.to_path_buf(),
        reason: err.to_string(),
    })?;

    let map = value
        .as_mapping()
        .cloned()
        .ok_or_else(|| WorkflowError::FrontMatterNotMap(path.to_path_buf()))?;

    Ok((map, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_optional_front_matter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("WORKFLOW.md");
        fs::write(&path, "---\ntracker:\n  kind: linear\n---\nHello").unwrap();

        let workflow = load_workflow(&path).unwrap();

        assert_eq!(workflow.prompt_template, "Hello");
        assert!(
            workflow
                .config
                .contains_key(Value::String("tracker".to_string()))
        );
    }

    #[test]
    fn rejects_non_map_front_matter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("WORKFLOW.md");
        fs::write(&path, "---\n- nope\n---\nHello").unwrap();

        assert!(matches!(
            load_workflow(&path),
            Err(WorkflowError::FrontMatterNotMap(_))
        ));
    }
}
