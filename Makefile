PG_CON ?= postgres://user:password@localhost:5432/galoy_agents
GITHUB_CLIENT_SECRET ?= $(shell echo $$GITHUB_CLIENT_SECRET)
ANTHROPIC_API_KEY ?= $(shell echo $$ANTHROPIC_API_KEY)

# ── Container engine ─────────────────────────────────────────────────────────────
# Set by the nix devShell shellHook. Override with: make start ENGINE_DEFAULT=docker
ENGINE_DEFAULT ?= $(shell command -v podman >/dev/null 2>&1 && echo podman || echo docker)
COMPOSE_CMD = $(ENGINE_DEFAULT) compose

clean-deps:
	$(COMPOSE_CMD) down -v

start-deps:
	$(COMPOSE_CMD) up -d

setup-db:
	@echo "Waiting for PostgreSQL..."
	@until $(COMPOSE_CMD) exec postgres pg_isready -U user -d galoy_agents > /dev/null 2>&1; do sleep 1; done
	@echo "PostgreSQL ready"
	DATABASE_URL=$(PG_CON) cargo sqlx migrate run --source core/migrations

reset-deps: clean-deps start-deps setup-db

sqlx-prepare:
	DATABASE_URL=$(PG_CON) cargo sqlx prepare --workspace -- --all-targets

build-sandbox:
	cargo build -p sandbox-tool-server

run-server: build-sandbox
	@PG_CON=$(PG_CON) GITHUB_CLIENT_SECRET=$(GITHUB_CLIENT_SECRET) ANTHROPIC_API_KEY=$(ANTHROPIC_API_KEY) cargo run -p galoy-agents-cli -- $(ARGS)

nix-run-server:
	@PG_CON=$(PG_CON) GITHUB_CLIENT_SECRET=$(GITHUB_CLIENT_SECRET) ANTHROPIC_API_KEY=$(ANTHROPIC_API_KEY) nix run . -- $(ARGS)

sdl-rust:
	SQLX_OFFLINE=true cargo run --bin write_sdl > web/src/graphql/schema.graphql

generate-default-config:
	SQLX_OFFLINE=true cargo run -q -p galoy-agents-cli -- dump-default-config > dev/galoy-agents.default.yml

integration-tests: reset-deps
	DATABASE_URL=$(PG_CON) cargo nextest run

start: reset-deps
	@PG_CON=$(PG_CON) ANTHROPIC_API_KEY=$(ANTHROPIC_API_KEY) cargo run -p galoy-agents-cli -- --set oauth.login=dev $(ARGS)
