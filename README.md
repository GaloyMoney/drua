# Drua

Drua runs AI agents with safe, scoped access to operational tools — Concourse
CI, Honeycomb, GitHub, Kubernetes, semantic code search, and arbitrary MCP
upstreams — behind one progressive-disclosure tool interface.

Agents execute in sandboxes (a local child process or an isolated K8s pod),
run as single prompts or multi-step YAML **workflows**, and expose their
projects, sessions, and runs through a GraphQL API and a terminal UI.

## Quick Start

```bash
# Requires Nix (https://nixos.org) + direnv
direnv allow          # loads the nix devShell + .env
cp .env.example .env  # then set a provider API key
make start            # Postgres + migrations + server on :4200 (dev login)
```

The web UI is at `http://localhost:4200`. `make start` uses dev login (no
GitHub OAuth). For GitHub OAuth in production, run `make run-server` with
`GITHUB_CLIENT_SECRET` set and an [OAuth App](https://github.com/settings/developers)
(callback `http://localhost:4200/auth/github/callback`).

## CLI

```bash
cargo run -p drua-cli -- tui                          # local dev server
cargo run -p drua-cli -- tui --server https://…       # remote
```

First run opens a browser to authenticate and stores a token in
`~/.drua/config.json`. Commands and key bindings: [`client/README.md`](client/README.md).

## Configuration

- **Secrets** → environment variables. Copy `.env.example`; at minimum set an
  LLM provider key (`ANTHROPIC_API_KEY` or `OPENAI_API_KEY`).
- **Non-secret config** → `drua.yml` (a full local-dev example ships in the
  repo). Override the path with `DRUA_CONFIG`.
- Full reference for every env var and config section:
  [`docs/configuration.md`](docs/configuration.md).

> **Heads up:** `openai` and `openai-responses` are *different OpenAI APIs*
> (Chat Completions vs Responses), not aliases. `openai-codex` reuses the
> Responses client against a ChatGPT subscription login. See
> [`docs/configuration.md`](docs/configuration.md#openai-providers) before
> swapping provider names.

## Project Layout

```
cli/             Binary entrypoint — dispatches to `server` or `tui`
client/          TUI: login, chat, projects, workflows
server/          Axum server: HTTP, auth, GraphQL, config
core/            Domain: agents, sessions, toolsets, sandbox, workflows, library
mcp-gateway/     MCP protocol gateway (rmcp-based)
lib/             Shared crates: LLM clients, sandbox admin, upstream clients, git-proxy, github-app, js-engine
images/          Container images: sandbox, tunnel-connector, concourse-drua-resource
code-assistant/  Semantic code search toolset
charts/          Helm chart · infra/ Terraform · ci/ Concourse pipeline · bats/ e2e tests
```

## Make Targets

The day-to-day ones:

| Target | Description |
|---|---|
| `make start` | `reset-deps` + server with dev login |
| `make run-server` | Build sandbox binary and start the server (prod login) |
| `make reset-deps` | `clean-deps` + `start-deps` + `setup-db` (Postgres) |
| `make reset-deps-native` | Same, but Postgres+pgvector runs natively from nix — no container VM |
| `make stop-deps-native` | Stop the native Postgres instance |
| `make sqlx-prepare` | Regenerate SQLx offline query data |
| `make sdl-rust` | Regenerate the GraphQL SDL (`server/src/graphql/schema.graphql`) |
| `make integration-tests` | `reset-deps` then `cargo nextest` |

See the `Makefile` for the full list (test-library helpers, fixture regen, etc.).
