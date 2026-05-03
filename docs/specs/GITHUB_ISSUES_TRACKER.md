# GitHub Issues Tracker Extension Specification

Status: Draft v0

Purpose: Define a GitHub Issues tracker adapter for Symphony with behavior as close as practical to the Linear adapter in `docs/specs/SPEC.md`.

## 1. Scope

This document extends the core Symphony service specification. It does not replace the core issue model, orchestrator rules, workspace safety invariants, retry semantics, or Codex runner behavior.

The extension adds support for `tracker.kind: github` so teams can run Symphony from GitHub Issues instead of Linear.

## 2. Goals

- Support GitHub Issues as a first-class issue tracker reader.
- Preserve the normalized `Issue` model used by orchestration and prompt rendering.
- Preserve active-state dispatch, terminal-state cleanup, retry handling, and reconciliation semantics.
- Support a low-friction default mode based on labels because GitHub Issues does not have Linear-style workflow states.
- Support GitHub Projects v2 status fields as a higher-fidelity optional mode.
- Document feature gaps and required workarounds explicitly.

## 3. Non-Goals

- Implement GitHub issue writes in the orchestrator.
- Require GitHub Projects v2 for basic operation.
- Require a single organization-wide issue workflow convention.
- Provide full parity with Linear-native priorities, blockers, or branch metadata when GitHub has no equivalent built-in field.

## 4. Configuration Schema

`tracker.kind` value:

- `github`

Core fields:

- `tracker.endpoint` (string)
  - Default: `https://api.github.com/graphql`
- `tracker.api_key` (string)
  - MAY be a literal token or `$VAR_NAME`.
  - Canonical environment variables: `GITHUB_TOKEN`, then `GH_TOKEN` if explicitly configured.
  - If `$VAR_NAME` resolves to an empty string, treat the key as missing.
- `tracker.owner` (string)
  - REQUIRED.
- `tracker.repo` (string)
  - REQUIRED for single-repository mode.
- `tracker.repositories` (list of objects, OPTIONAL)
  - OPTIONAL multi-repository mode.
  - Each object contains `owner` and `repo`.
  - If present, it replaces `tracker.owner` plus `tracker.repo` for candidate fetches.
- `tracker.active_states` (list of strings)
  - Default for label/project status modes: `Todo`, `In Progress`.
  - Default for native mode: `Open`.
- `tracker.terminal_states` (list of strings)
  - Default for label/project status modes: `Done`, `Closed`, `Cancelled`, `Canceled`, `Duplicate`.
  - Default for native mode: `Closed`.

GitHub-specific fields under `tracker.github`:

- `state_source` (string)
  - Supported values: `labels`, `project_v2`, `native`.
  - Default: `labels`.
- `state_label_prefix` (string)
  - Default: `status:`.
  - Used only when `state_source: labels`.
- `project_owner` (string, OPTIONAL)
  - REQUIRED when `state_source: project_v2` unless inferable from `tracker.owner`.
- `project_owner_kind` (string, OPTIONAL)
  - Supported values: `user`, `organization`.
  - Default: implementation-defined.
- `project_number` (integer, OPTIONAL)
  - REQUIRED when `state_source: project_v2`.
- `project_status_field` (string)
  - Default: `Status`.
- `priority_source` (string)
  - Supported values: `labels`, `project_v2`, `none`.
  - Default: `labels`.
- `priority_label_prefix` (string)
  - Default: `priority:`.
- `blockers_source` (string)
  - Supported values: `body_references`, `none`.
  - Default: `body_references`.
- `blockers_heading` (string)
  - Default: `Blocked by`.
- `branch_name_source` (string)
  - Supported values: `derived`, `none`.
  - Default: `derived`.

Example label-backed workflow:

```yaml
tracker:
  kind: github
  api_key: $GITHUB_TOKEN
  owner: lightcap
  repo: symphony-rust
  active_states:
    - Todo
    - In Progress
  terminal_states:
    - Done
    - Closed
    - Cancelled
  github:
    state_source: labels
    state_label_prefix: "status:"
    priority_source: labels
    priority_label_prefix: "priority:"
    blockers_source: body_references
```

Example Projects v2-backed workflow:

```yaml
tracker:
  kind: github
  api_key: $GITHUB_TOKEN
  owner: lightcap
  repo: symphony-rust
  active_states:
    - Todo
    - In Progress
  terminal_states:
    - Done
  github:
    state_source: project_v2
    project_owner: lightcap
    project_owner_kind: user
    project_number: 3
    project_status_field: Status
    priority_source: project_v2
```

## 5. Normalized Issue Mapping

GitHub adapter output MUST match the core `Issue` model.

- `id`
  - GitHub GraphQL node ID for the issue.
- `identifier`
  - `OWNER/REPO#NUMBER`.
  - This avoids collisions in multi-repository mode.
- `title`
  - GitHub issue title.
- `description`
  - GitHub issue body, or null.
- `priority`
  - Integer parsed from configured priority source, or null.
  - Lower numbers are higher priority.
- `state`
  - Logical Symphony state derived from the configured state source.
- `branch_name`
  - Derived from issue number/title when `branch_name_source: derived`, otherwise null.
- `url`
  - GitHub issue URL.
- `labels`
  - GitHub label names normalized to lowercase.
- `blocked_by`
  - Blocker references from configured blocker source.
- `created_at`
  - GitHub issue `createdAt`.
- `updated_at`
  - GitHub issue `updatedAt`.

## 6. State Sources

### 6.1 Labels Mode

Labels mode is the default because it works for normal GitHub repositories without requiring Projects v2.

State derivation:

- Inspect issue labels for a label beginning with `tracker.github.state_label_prefix`.
- Remove the prefix and trim whitespace to get the logical state.
- Example: label `status: In Progress` maps to state `In Progress`.
- If multiple state labels are present, behavior is implementation-defined and MUST be documented. The recommended behavior is to choose the first label by GitHub API order and log a warning.
- If no state label is present, use native GitHub state fallback: `Open` or `Closed`.

Candidate fetch:

- Fetch issues with native state `OPEN` and filter client-side by logical active states.
- Implementations MAY use GitHub search or label filters for efficiency, but MUST preserve label-state semantics.

Terminal cleanup:

- Fetch issues with native state `CLOSED` and issues carrying terminal state labels when feasible.
- Remove workspaces whose derived logical state is terminal.

### 6.2 Projects v2 Mode

Projects v2 mode gives the closest parity to Linear workflow states.

State derivation:

- Query the configured Project v2 item for the issue.
- Read the single-select field named by `project_status_field`.
- Use that field value as the logical state.
- If the issue is not in the project or the status field is missing, use native GitHub state fallback.

Candidate fetch:

- Prefer querying project items directly and selecting content nodes whose content type is `Issue`.
- Ignore draft issues and non-issue project items.
- Filter by repository when `tracker.repo` or `tracker.repositories` is configured.

Terminal cleanup:

- Query project items in terminal statuses when practical.
- Also consider native `CLOSED` issues terminal when `Closed` is in `terminal_states`.

### 6.3 Native Mode

Native mode maps GitHub's built-in issue state directly.

State derivation:

- GitHub `OPEN` maps to `Open`.
- GitHub `CLOSED` maps to `Closed`.
- `stateReason` MAY be appended or exposed as an extension field, but MUST NOT change core `state` unless documented.

Native mode has the least workflow parity and is best for simple queues.

## 7. Priority Mapping

Labels mode:

- Parse labels matching `priority_label_prefix` followed by an integer.
- Examples: `priority: 1`, `priority:1`, `priority: P1`.
- Implementations SHOULD accept `p1`/`P1` as `1`.
- If multiple priority labels exist, choose the lowest numeric value and log a warning.

Projects v2 mode:

- Read a configured priority field if implemented.
- Numeric fields map directly.
- Single-select values SHOULD parse leading or embedded priority numbers.

If no priority is found, set `priority` to null.

## 8. Blocker Mapping

GitHub Issues does not expose a universally available inverse blocker relation equivalent to Linear's inverse `blocks` relation.

Default body reference format:

```markdown
Blocked by: #123, owner/other-repo#456
```

Rules:

- Parse the configured `blockers_heading` line from the issue body.
- Accept same-repository references like `#123`.
- Accept cross-repository references like `OWNER/REPO#123`.
- Fetch referenced issues and populate `blocked_by` with ID, identifier, and logical state when accessible.
- If a referenced issue cannot be fetched, include a blocker ref with best-effort identifier and null state.
- The core Todo blocker rule still applies: Todo issues are not dispatched while any blocker is non-terminal or unknown.

Optional future sources:

- GitHub issue dependencies or sub-issues APIs if they become broadly available and stable through GraphQL.
- Project v2 custom fields containing blocker issue references.

## 9. Required Tracker Operations

The GitHub adapter MUST implement the same operations as the core tracker contract.

### 9.1 `fetch_candidate_issues()`

Return normalized issues in configured active states.

Requirements:

- Exclude pull requests.
- Include labels.
- Include body when blocker parsing is enabled.
- Include project item status fields when `state_source: project_v2`.
- Page through all relevant results.

### 9.2 `fetch_issues_by_states(state_names)`

Used for startup terminal cleanup.

Requirements:

- Return normalized issues whose derived logical state is in `state_names`.
- Include closed issues when `Closed` is requested.
- Include label/project terminal states when configured.

### 9.3 `fetch_issue_states_by_ids(issue_ids)`

Used for active-run reconciliation.

Requirements:

- Use GitHub GraphQL `nodes(ids: [...])` when possible.
- Recompute logical state using the configured state source.
- Return a map keyed by GraphQL issue ID.

## 10. GitHub API Requirements

Recommended API:

- GitHub GraphQL API v4.

Authentication:

- Send `Authorization: Bearer <token>`.
- Public repositories can use a token with public repository read access.
- Private repositories require repository issue read access.
- Projects v2 mode requires project read access.

Pagination:

- Page size default: `50`.
- Pagination is REQUIRED for candidate and cleanup fetches.
- Implementations MUST detect missing cursors when `hasNextPage` is true.

Timeout:

- Network timeout default: `30000 ms`.

Recommended error categories:

- `unsupported_tracker_kind`
- `missing_tracker_api_key`
- `missing_github_owner`
- `missing_github_repo`
- `missing_github_project_number`
- `github_api_request`
- `github_api_status`
- `github_graphql_errors`
- `github_unknown_payload`
- `github_missing_end_cursor`
- `github_rate_limited`

## 11. Optional Client-Side Tool Extension

An implementation MAY expose a `github_graphql` client-side tool to Codex sessions.

Purpose:

- Execute one raw GraphQL query or mutation against GitHub using Symphony's configured tracker auth.

Input shape:

```json
{
  "query": "single GraphQL query or mutation document",
  "variables": {
    "optional": "graphql variables object"
  }
}
```

Rules:

- `query` MUST be a non-empty string.
- `query` MUST contain exactly one GraphQL operation.
- `variables` is OPTIONAL and, when present, MUST be a JSON object.
- Raw GraphQL query string shorthand MAY be accepted.
- Reuse the configured GitHub endpoint and token.
- Top-level GraphQL errors return `success=false` while preserving the response body.

## 12. Roadblocks And Workarounds

### Workflow States

Roadblock: GitHub Issues only has native `OPEN` and `CLOSED` states.

Workarounds:

- Use labels such as `status: Todo`, `status: In Progress`, and `status: Done`.
- Use GitHub Projects v2 single-select `Status` for closer Linear parity.
- Use native mode only for simple open/closed queues.

### Priorities

Roadblock: GitHub Issues has no native priority field.

Workarounds:

- Use labels such as `priority: 1` or `priority: P1`.
- Use a Projects v2 priority field.
- Leave priority null and rely on creation time sorting.

### Blockers

Roadblock: GitHub Issues does not provide a broadly available Linear-style inverse blocker relation through the stable issue APIs.

Workarounds:

- Parse `Blocked by:` references from issue bodies.
- Use a Project v2 field or issue form field by convention.
- Require agents/humans to keep blocker references current.

### Branch Metadata

Roadblock: GitHub Issues has no issue-owned branch metadata equivalent to Linear's branch name field.

Workarounds:

- Derive a branch name from `OWNER/REPO#NUMBER` and the issue title.
- Let workflow hooks create/check out branches according to repository policy.
- Infer branch names from linked PRs only as a best-effort extension.

### Terminal Cleanup Semantics

Roadblock: A closed GitHub issue may not mean the same thing as a terminal workflow state, and a terminal label may exist on an open issue.

Workarounds:

- Treat configured terminal labels/project statuses as terminal regardless of native open/closed state.
- Treat native `Closed` as terminal only when `Closed` is included in `terminal_states`.
- Document repository-specific handoff states in `WORKFLOW.md`.

### Projects v2 Complexity

Roadblock: Projects v2 GraphQL queries are more complex, permission-sensitive, and may be organization-scoped rather than repository-scoped.

Workarounds:

- Make labels mode the default.
- Implement Projects v2 as an optional higher-fidelity mode.
- Fail startup with clear validation errors when project configuration or token scopes are insufficient.

### Query Efficiency And Limits

Roadblock: Fetching all open issues and filtering client-side is simple but can be inefficient for large repositories. GitHub search has rate limits and result caps.

Workarounds:

- Use label filters or search queries as an optimization when safe.
- Keep page size configurable in a future extension.
- Encourage status labels on active queues so implementations can reduce the candidate set.

### Cross-Repository Workflows

Roadblock: GitHub issue numbers are repository-local, and blockers can reference issues across repositories.

Workarounds:

- Use `OWNER/REPO#NUMBER` as the normalized identifier.
- Require explicit `tracker.repositories` for multi-repository dispatch.
- Resolve cross-repository blocker refs best-effort and treat inaccessible blockers as non-terminal.

### Permissions

Roadblock: Token permissions vary across classic PATs, fine-grained PATs, GitHub App tokens, public repos, private repos, and Projects v2.

Workarounds:

- Validate token access during startup preflight.
- Document minimum scopes for each state source.
- Prefer GitHub App installation tokens for production automation when available.

## 13. Parity Summary

High parity with Linear:

- Candidate polling.
- Startup terminal cleanup.
- Active-run state reconciliation.
- Per-issue workspace isolation.
- Prompt rendering with normalized issue data.
- Labels.
- URLs.
- Created/updated timestamps.

Partial parity with Linear:

- Workflow state, via labels or Projects v2.
- Priority, via labels or Projects v2.
- Blockers, via body/project conventions.
- Branch name, via derivation or hooks.

Not currently equivalent:

- Linear's native project slug maps only loosely to GitHub repository or Project v2 configuration.
- Linear's inverse issue relations have no stable universal GitHub Issues equivalent.
- Linear's issue branch metadata has no native GitHub Issues equivalent.

## 14. Implementation Plan

1. Add typed config support for `tracker.kind: github` and `tracker.github` fields.
2. Add a `GitHubTracker` implementing the existing `IssueTracker` trait.
3. Implement labels mode first because it requires the fewest permissions and works in normal repositories.
4. Add body-reference blocker parsing and referenced issue state lookup.
5. Add optional `github_graphql` Codex client-side tool.
6. Add Projects v2 status mode after labels mode is stable.
7. Add integration tests gated by explicit GitHub credentials and repository configuration.
