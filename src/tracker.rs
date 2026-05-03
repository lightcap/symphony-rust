use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde_json::{Value, json};
use thiserror::Error;

use crate::config::ServiceConfig;
use crate::model::{BlockerRef, Issue, IssueState};

const PAGE_SIZE: i64 = 50;

#[derive(Debug, Error)]
pub enum TrackerError {
    #[error("unsupported_tracker_kind kind={0}")]
    UnsupportedTrackerKind(String),
    #[error("missing_tracker_api_key")]
    MissingApiKey,
    #[error("missing_tracker_project_slug")]
    MissingProjectSlug,
    #[error("linear_api_request reason={0}")]
    LinearApiRequest(String),
    #[error("linear_api_status status={0}")]
    LinearApiStatus(StatusCode),
    #[error("linear_graphql_errors errors={0}")]
    LinearGraphqlErrors(Value),
    #[error("linear_unknown_payload reason={0}")]
    LinearUnknownPayload(String),
    #[error("linear_missing_end_cursor")]
    LinearMissingEndCursor,
}

#[async_trait]
pub trait IssueTracker: Send + Sync {
    async fn fetch_candidate_issues(
        &self,
        config: &ServiceConfig,
    ) -> Result<Vec<Issue>, TrackerError>;

    async fn fetch_issues_by_states(
        &self,
        state_names: &[String],
        config: &ServiceConfig,
    ) -> Result<Vec<Issue>, TrackerError>;

    async fn fetch_issue_states_by_ids(
        &self,
        issue_ids: &[String],
        config: &ServiceConfig,
    ) -> Result<HashMap<String, IssueState>, TrackerError>;
}

#[derive(Clone, Debug)]
pub struct LinearTracker {
    client: reqwest::Client,
}

impl LinearTracker {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_millis(30_000))
                .build()
                .expect("reqwest client configuration is valid"),
        }
    }

    async fn graphql(
        &self,
        config: &ServiceConfig,
        query: &str,
        variables: Value,
    ) -> Result<Value, TrackerError> {
        if config.tracker.kind.as_deref() != Some("linear") {
            return Err(TrackerError::UnsupportedTrackerKind(
                config.tracker.kind.clone().unwrap_or_default(),
            ));
        }
        let api_key = config
            .tracker
            .api_key
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or(TrackerError::MissingApiKey)?;

        let response = self
            .client
            .post(&config.tracker.endpoint)
            .bearer_auth(api_key)
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await
            .map_err(|err| TrackerError::LinearApiRequest(err.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            return Err(TrackerError::LinearApiStatus(status));
        }

        let payload = response
            .json::<Value>()
            .await
            .map_err(|err| TrackerError::LinearApiRequest(err.to_string()))?;

        if let Some(errors) = payload.get("errors") {
            return Err(TrackerError::LinearGraphqlErrors(errors.clone()));
        }

        Ok(payload)
    }
}

impl Default for LinearTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl IssueTracker for LinearTracker {
    async fn fetch_candidate_issues(
        &self,
        config: &ServiceConfig,
    ) -> Result<Vec<Issue>, TrackerError> {
        self.fetch_issues_by_states(&config.tracker.active_states, config)
            .await
    }

    async fn fetch_issues_by_states(
        &self,
        state_names: &[String],
        config: &ServiceConfig,
    ) -> Result<Vec<Issue>, TrackerError> {
        let project_slug = config
            .tracker
            .project_slug
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or(TrackerError::MissingProjectSlug)?;
        let mut after: Option<String> = None;
        let mut issues = Vec::new();

        loop {
            let variables = json!({
                "projectSlug": project_slug,
                "stateNames": state_names,
                "first": PAGE_SIZE,
                "after": after,
            });
            let payload = self
                .graphql(config, CANDIDATE_ISSUES_QUERY, variables)
                .await?;
            let page = payload.pointer("/data/issues").ok_or_else(|| {
                TrackerError::LinearUnknownPayload("missing data.issues".to_string())
            })?;

            let nodes = page.get("nodes").and_then(Value::as_array).ok_or_else(|| {
                TrackerError::LinearUnknownPayload("missing issues.nodes".to_string())
            })?;
            issues.extend(nodes.iter().filter_map(normalize_issue));

            let page_info = page.get("pageInfo").ok_or_else(|| {
                TrackerError::LinearUnknownPayload("missing issues.pageInfo".to_string())
            })?;
            let has_next = page_info
                .get("hasNextPage")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !has_next {
                break;
            }
            after = page_info
                .get("endCursor")
                .and_then(Value::as_str)
                .map(str::to_string);
            if after.is_none() {
                return Err(TrackerError::LinearMissingEndCursor);
            }
        }

        Ok(issues)
    }

    async fn fetch_issue_states_by_ids(
        &self,
        issue_ids: &[String],
        config: &ServiceConfig,
    ) -> Result<HashMap<String, IssueState>, TrackerError> {
        if issue_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let payload = self
            .graphql(
                config,
                ISSUE_STATES_QUERY,
                json!({ "ids": issue_ids, "first": issue_ids.len().max(1) as i64 }),
            )
            .await?;
        let nodes = payload
            .pointer("/data/issues/nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                TrackerError::LinearUnknownPayload("missing data.issues.nodes".to_string())
            })?;

        Ok(nodes
            .iter()
            .filter_map(|node| {
                let id = node.get("id")?.as_str()?.to_string();
                let state = node.pointer("/state/name")?.as_str()?.to_string();
                let identifier = node
                    .get("identifier")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Some((
                    id.clone(),
                    IssueState {
                        id,
                        identifier,
                        state,
                    },
                ))
            })
            .collect())
    }
}

pub async fn linear_graphql_tool(
    config: &ServiceConfig,
    query: &str,
    variables: Option<Value>,
) -> Result<Value, TrackerError> {
    validate_single_graphql_operation(query)?;
    let tracker = LinearTracker::new();
    tracker
        .graphql(config, query, variables.unwrap_or_else(|| json!({})))
        .await
}

fn validate_single_graphql_operation(query: &str) -> Result<(), TrackerError> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Err(TrackerError::LinearUnknownPayload(
            "empty GraphQL query".to_string(),
        ));
    }

    let operation_count = trimmed
        .split(|ch: char| !ch.is_ascii_alphabetic())
        .filter(|token| matches!(*token, "query" | "mutation" | "subscription"))
        .count();

    if operation_count > 1 {
        return Err(TrackerError::LinearUnknownPayload(
            "linear_graphql accepts exactly one operation".to_string(),
        ));
    }

    Ok(())
}

fn normalize_issue(node: &Value) -> Option<Issue> {
    let id = node.get("id")?.as_str()?.to_string();
    let identifier = node.get("identifier")?.as_str()?.to_string();
    let title = node.get("title")?.as_str()?.to_string();
    let state = node.pointer("/state/name")?.as_str()?.to_string();

    Some(Issue {
        id,
        identifier,
        title,
        description: node
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        priority: node.get("priority").and_then(Value::as_i64),
        state,
        branch_name: node
            .get("branchName")
            .and_then(Value::as_str)
            .map(str::to_string),
        url: node.get("url").and_then(Value::as_str).map(str::to_string),
        labels: labels(node),
        blocked_by: blockers(node),
        created_at: parse_time(node.get("createdAt")),
        updated_at: parse_time(node.get("updatedAt")),
    })
}

fn labels(node: &Value) -> Vec<String> {
    node.pointer("/labels/nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|label| label.get("name").and_then(Value::as_str))
        .map(|name| name.to_lowercase())
        .collect()
}

fn blockers(node: &Value) -> Vec<BlockerRef> {
    node.pointer("/inverseRelations/nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|relation| {
            relation
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.eq_ignore_ascii_case("blocks"))
        })
        .filter_map(|relation| {
            relation
                .get("issue")
                .or_else(|| relation.get("relatedIssue"))
        })
        .map(|issue| BlockerRef {
            id: issue.get("id").and_then(Value::as_str).map(str::to_string),
            identifier: issue
                .get("identifier")
                .and_then(Value::as_str)
                .map(str::to_string),
            state: issue
                .pointer("/state/name")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
        .collect()
}

fn parse_time(value: Option<&Value>) -> Option<DateTime<Utc>> {
    let value = value?.as_str()?;
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

const CANDIDATE_ISSUES_QUERY: &str = r#"
query SymphonyCandidateIssues($projectSlug: String!, $stateNames: [String!], $first: Int!, $after: String) {
  issues(
    first: $first
    after: $after
    filter: {
      project: { slugId: { eq: $projectSlug } }
      state: { name: { in: $stateNames } }
    }
  ) {
    pageInfo { hasNextPage endCursor }
    nodes {
      id
      identifier
      title
      description
      priority
      branchName
      url
      createdAt
      updatedAt
      state { name }
      labels { nodes { name } }
      inverseRelations { nodes { type issue { id identifier state { name } } relatedIssue { id identifier state { name } } } }
    }
  }
}
"#;

const ISSUE_STATES_QUERY: &str = r#"
query SymphonyIssueStates($ids: [ID!], $first: Int!) {
  issues(first: $first, filter: { id: { in: $ids } }) {
    nodes { id identifier state { name } }
  }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_issue_payload() {
        let node = json!({
            "id": "id",
            "identifier": "ABC-1",
            "title": "Fix",
            "description": null,
            "priority": 1,
            "branchName": "abc-1",
            "url": "https://linear.app/x/issue/ABC-1",
            "createdAt": "2026-01-01T00:00:00Z",
            "updatedAt": "2026-01-02T00:00:00Z",
            "state": { "name": "Todo" },
            "labels": { "nodes": [{ "name": "Bug" }] },
            "inverseRelations": { "nodes": [{ "type": "blocks", "issue": { "id": "b", "identifier": "ABC-0", "state": { "name": "Todo" } } }] }
        });

        let issue = normalize_issue(&node).unwrap();

        assert_eq!(issue.labels, vec!["bug"]);
        assert_eq!(issue.blocked_by[0].identifier.as_deref(), Some("ABC-0"));
    }

    #[test]
    fn rejects_multiple_graphql_operations() {
        assert!(
            validate_single_graphql_operation("query A { viewer { id } } mutation B { x }")
                .is_err()
        );
    }
}
