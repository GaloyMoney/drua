# Drua

An MCP-gateway platform that orchestrates AI agents with tool access across
Concourse CI, Honeycomb observability, GitHub source control, Kubernetes, and
code-search — all behind a unified progressive-disclosure interface.

## Quick Start

```bash
# 1. Install Nix (https://nixos.org) and direnv, then:
direnv allow          # loads the nix devShell + .env

# 2. Copy and fill in secrets
cp .env.example .env
$EDITOR .env          # at minimum set the key/token for the provider you use

# 3. Start everything (Postgres, migrations, server with dev login)
make start            # full reset + server on :4200
```

The web UI is at `http://localhost:4200`. `make start` uses dev login mode
(no GitHub OAuth required). For production GitHub OAuth, use `make run-server`
with `GITHUB_CLIENT_SECRET` set and an
[OAuth App](https://github.com/settings/developers) configured with callback
`http://localhost:4200/auth/github/callback`.

## CLI (`drua`)

`drua` is a terminal UI for managing workspaces and chatting with agents.

```bash
# Against the local dev server
cargo run -p drua -- dashboard

# Against production
cargo run -p drua -- --server https://dashboard.agents.galoy.io dashboard
```

On first run it opens your browser to authenticate and generate an API token.
Credentials are stored in `~/.drua/config.json`. See [`drua/README.md`](drua/README.md)
for all commands and key bindings.

## Environment Variables

All secrets are loaded from environment variables (or CLI flags). Non-secret
configuration lives in a YAML config file (`drua.yml` by default).
A complete `.env.example` is provided at the repo root.

### Main Server (`drua-cli`)

| Variable | Required | Default | Description |
|---|---|---|---|
| `PG_CON` | No | `postgres://user:password@localhost:5432/drua` | PostgreSQL connection URL. The Makefile provides this default, which matches the bundled compose stack. |
| `GITHUB_CLIENT_SECRET` | No | `dev-secret` | GitHub OAuth App client secret (only needed when `oauth.login: github`). |
| `ANTHROPIC_API_KEY` | No | `""` | Anthropic API key for the agent LLM runtime. Server starts without it but agent prompts will fail. |
| `OPENAI_API_KEY` | No | `""` | OpenAI Platform API key used by `openai` (Chat Completions API) and `openai-responses` (Responses API). |
| `OPENAI_CODEX_ACCESS_TOKEN` | No | `""` | Optional override for `openai-codex`. If unset, Drua reads the cached Codex/ChatGPT login from `~/.codex/auth.json`. |
| `DRUA_CONFIG` | No | `drua.yml` | Path to the YAML config file. |
| `GITHUB_ALLOWED_TEAMS` | No | `""` (all users) | Comma-separated GitHub teams allowed to log in (`org/team-slug`). |
| `CONCOURSE_USERNAME` | No | — | Concourse CI basic-auth username (when concourse toolset is enabled). |
| `CONCOURSE_PASSWORD` | No | — | Concourse CI basic-auth password. |
| `{UPSTREAM}_AUTH_HEADER` | No | — | Auth header for each MCP upstream. Name is uppercased from the config, e.g. `HONEYCOMB_AUTH_HEADER`, `GITHUB_AUTH_HEADER`, `LINGO_AUTH_HEADER`. |
| `GITHUB_APP_PRIVATE_KEY_PATH` | No | — | Filesystem path to the GitHub App PEM private key (for sandbox token auto-provisioning). Requires `github_app` section in config. |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | No | `http://localhost:4317` | OpenTelemetry OTLP gRPC endpoint. |
| `OTEL_SDK_DISABLED` | No | `false` | Set to `true` to disable OpenTelemetry tracing entirely. |
| `RUST_LOG` | No | `info` | Standard `tracing` / `EnvFilter` log level directive. |

### Sandbox Tool Server (`sandbox-tool-server`)

These variables are set **automatically** by the local sandbox spawner (or by
the K8s pod spec). You only need them when running the sandbox server directly.

| Variable | Required | Default | Description |
|---|---|---|---|
| `PORT` | No | `3000` | HTTP listen port. |
| `WORKSPACE_ROOT` | No | `/workspace` | Root directory for sandbox file operations. |
| `GITHUB_TOKEN_PATH` | No | `/run/secrets/github-token` | Path to the GitHub token file (injected by the platform). |

### Test-Only Variables

These are only needed when running specific integration tests:

| Variable | Test File | Description |
|---|---|---|
| `ANTHROPIC_API_KEY` | `core/tests/prompt_executor.rs` | Anthropic round-trip test. |
| `DATABASE_URL` | `core/tests/agent.rs` | Postgres URL for agent integration tests. |
| `HONEYCOMB_AUTH_HEADER` | `core/tests/toolset.rs` | Auth header for MCP upstream toolset test. |

## Config File Reference

The YAML config file (`drua.yml`) holds non-secret configuration. See
the included `drua.yml` for a complete local-dev example. Key sections:

| Section | Purpose |
|---|---|
| `server` | Host, port, secure cookies, MCP endpoint URL |
| `oauth` | GitHub OAuth client ID, redirect URI, allowed teams |
| `agents.builtin_roles` | Per-role LLM model, system prompt, max tokens, auto-reset timer |
| `toolsets.concourse` | Concourse CI URL, team, enabled flag |
| `toolsets.mcp_upstreams[]` | MCP upstream services (name, URL, category, allowed tools, auth header name) |
| `toolsets.code_assistant` | Path to the code-assistant SQLite DB |
| `sandbox.backend` | `local` (child process) or `k8s` (namespace + template) |
| `github_app` | GitHub App client ID and installation ID (private key via env) |

### OpenAI Providers

Drua exposes two OpenAI API protocols, and they are not aliases:

| Provider name | API | Auth | Notes |
|---|---|---|---|
| `openai` | Chat Completions API | `OPENAI_API_KEY` | Uses `https://api.openai.com/v1/chat/completions`. |
| `openai-responses` | Responses API | `OPENAI_API_KEY` | Uses `https://api.openai.com/v1/responses`. |
| `openai-codex` | Responses API | ChatGPT subscription login | Uses the same Responses protocol as `openai-responses`, but against the ChatGPT/Codex subscription endpoint with credentials from `OPENAI_CODEX_ACCESS_TOKEN` or `~/.codex/auth.json`. |

`openai` and `openai-responses` are different OpenAI APIs with different wire
formats, streaming events, and tool-calling shapes. Choose the provider name
that matches the API you want to talk to; switching between them is not just a
billing or model change.

Example config:

```yaml
providers:
  - name: openai
    models:
      - name: gpt-4.1-mini
        max_tokens_per_response: 4096
        context_window_tokens: 200000

  - name: openai-responses
    models:
      - name: gpt-5.4-mini
        max_tokens_per_response: 4096
        context_window_tokens: 200000

  # Same Responses API client, but authenticated with a ChatGPT subscription.
  - name: openai-codex
    models:
      - name: gpt-5.4-mini
        max_tokens_per_response: 4096
        context_window_tokens: 200000
```

## Project Layout

```
drua/           Terminal UI client (login, dashboard, workspace management)
cli/            CLI entrypoint (config loading, main server binary)
core/           Domain logic (agents, sessions, toolsets, sandbox, encryption)
web/            Axum web server (routes, OAuth, templates)
mcp-gateway/    MCP protocol gateway (rmcp-based)
lib/            Shared libraries (anthropic-client, sandbox admin client, LLM)
images/sandbox/ Sandbox tool server (runs inside sandbox pods)
code-assistant/ Semantic code search toolset
charts/         Helm chart for Kubernetes deployment
```

## Make Targets

| Target | Description |
|---|---|
| `make start-deps` | Start Postgres via docker/podman compose |
| `make clean-deps` | Tear down Postgres and volumes |
| `make setup-db` | Wait for Postgres and run SQLx migrations |
| `make reset-deps` | `clean-deps` + `start-deps` + `setup-db` |
| `make run-server` | Build sandbox binary and start the server |
| `make nix-run-server` | Run server via `nix run .` |
| `make build-sandbox` | Build only the sandbox tool server |
| `make sqlx-prepare` | Regenerate SQLx offline query data |
| `make start` | Full reset: `reset-deps` then server with dev login |
