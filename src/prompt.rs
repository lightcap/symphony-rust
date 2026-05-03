use minijinja::{Environment, UndefinedBehavior, context};
use thiserror::Error;

use crate::model::Issue;

const DEFAULT_PROMPT: &str = "You are working on an issue from Linear.";

#[derive(Debug, Error)]
pub enum PromptError {
    #[error("template_parse_error reason={0}")]
    Parse(String),
    #[error("template_render_error reason={0}")]
    Render(String),
}

pub fn render_prompt(
    prompt_template: &str,
    issue: &Issue,
    attempt: Option<u32>,
) -> Result<String, PromptError> {
    let source = if prompt_template.trim().is_empty() {
        DEFAULT_PROMPT
    } else {
        prompt_template
    };

    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.add_template("workflow", source)
        .map_err(|err| PromptError::Parse(err.to_string()))?;
    let template = env
        .get_template("workflow")
        .map_err(|err| PromptError::Parse(err.to_string()))?;

    template
        .render(context! { issue => issue, attempt => attempt })
        .map_err(|err| PromptError::Render(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue() -> Issue {
        Issue {
            id: "id".to_string(),
            identifier: "ABC-1".to_string(),
            title: "Fix it".to_string(),
            description: None,
            priority: Some(1),
            state: "Todo".to_string(),
            branch_name: None,
            url: None,
            labels: vec!["bug".to_string()],
            blocked_by: Vec::new(),
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn renders_issue_fields_strictly() {
        let rendered = render_prompt(
            "Work on {{ issue.identifier }}: {{ issue.title }}",
            &issue(),
            None,
        )
        .unwrap();

        assert_eq!(rendered, "Work on ABC-1: Fix it");
    }

    #[test]
    fn fails_unknown_variables() {
        let err = render_prompt("{{ issue.nope }}", &issue(), None).unwrap_err();

        assert!(matches!(err, PromptError::Render(_)));
    }
}
