---
tracker:
  kind: linear
  api_key: $LINEAR_API_KEY
  project_slug: your-linear-project-slug
  active_states:
    - Todo
    - In Progress
  terminal_states:
    - Closed
    - Cancelled
    - Canceled
    - Duplicate
    - Done
polling:
  interval_ms: 30000
workspace:
  root: .symphony_workspaces
agent:
  max_concurrent_agents: 2
  max_turns: 20
  max_retry_backoff_ms: 300000
codex:
  command: codex app-server
  approval_policy: never
  turn_timeout_ms: 3600000
  read_timeout_ms: 5000
  stall_timeout_ms: 300000
server:
  port: 8080
---
You are working on Linear issue {{ issue.identifier }}: {{ issue.title }}.

Description:
{{ issue.description }}

Labels: {{ issue.labels }}

Follow the repository workflow. When implementation and validation are complete, update the ticket with the next handoff state and a concise summary.
