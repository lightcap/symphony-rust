# Symphony Rust

Rust implementation of the draft Symphony service specification. It runs a long-lived scheduler that reads Linear issues, creates per-issue workspaces, and drives Codex app-server sessions inside those workspaces.

## Run

```sh
cargo run -- path/to/WORKFLOW.md
```

If the workflow path is omitted, the service uses `./WORKFLOW.md`.

Local `.env` files are loaded automatically from the current directory before startup validation. Existing process environment variables win over values in `.env`.

Copy `.env.example` to `.env` and fill in local secrets:

```sh
cp .env.example .env
```

Enable the optional HTTP status surface with `--port` or `server.port` in `WORKFLOW.md`:

```sh
cargo run -- WORKFLOW.md --port 8080
```

The HTTP extension binds `127.0.0.1` and serves:

- `GET /`
- `GET /api/v1/state`
- `GET /api/v1/{issue_identifier}`
- `POST /api/v1/refresh`

If you use `mise`, the repo includes convenience tasks:

```sh
mise run test
mise run lint
mise run verify
mise run symphony
```

Set `SYMPHONY_WORKFLOW` and `SYMPHONY_PORT` to customize the `symphony` task.

## Implementation-Defined Policy

This implementation targets trusted developer automation environments.

- Codex approval defaults are passed through unless configured in `WORKFLOW.md`.
- Command and file-change approval requests are auto-approved for the session.
- User-input and elicitation requests fail the run immediately so sessions do not stall indefinitely.
- Unsupported dynamic tool calls receive a JSON-RPC error or failed tool result and the session continues where the Codex protocol allows.
- The optional `linear_graphql` tool uses the active Linear endpoint and token from the loaded workflow config.
- Workspaces are created/reused under `workspace.root`; successful runs preserve workspaces.
- Existing non-directory workspace paths are treated as fatal workspace errors.
- No built-in VCS reset or workspace population is performed; use `hooks.after_create` and `hooks.before_run` for checkout/bootstrap policy.
- Terminal-state cleanup deletes only the sanitized per-issue workspace under the configured workspace root after running `hooks.before_remove` best-effort.

## Safety Invariants

- Workspace keys replace characters outside `[A-Za-z0-9._-]` with `_`.
- Workspace paths are normalized and must remain under `workspace.root`.
- Codex app-server is launched with the per-issue workspace as its subprocess working directory.

## Notes

The Codex app-server client uses the generated JSON-RPC v2 method names available in current Codex CLI builds: `initialize`, `thread/start`, and `turn/start`. Codex-owned policy values such as `approval_policy`, `thread_sandbox`, and `turn_sandbox_policy` are passed through as workflow-provided JSON values.

The upstream Symphony specification is vendored at `docs/specs/SPEC.md` for implementation reference.
