PG_CON ?= postgres://user:password@localhost:5432/galoy_agents
GITHUB_CLIENT_SECRET ?= dev-secret
ANTHROPIC_API_KEY ?= $(shell echo $$ANTHROPIC_API_KEY)

clean-deps:
	docker compose down -v

start-deps:
	docker compose up -d

setup-db:
	@echo "Waiting for PostgreSQL..."
	@until docker compose exec postgres pg_isready -U user -d galoy_agents > /dev/null 2>&1; do sleep 1; done
	@echo "PostgreSQL ready"
	DATABASE_URL=$(PG_CON) cargo sqlx migrate run --source core/migrations

reset-deps: clean-deps start-deps setup-db

sqlx-prepare:
	DATABASE_URL=$(PG_CON) cargo sqlx prepare --workspace -- --all-targets

run-server:
	@PG_CON=$(PG_CON) GITHUB_CLIENT_SECRET=$(GITHUB_CLIENT_SECRET) ANTHROPIC_API_KEY=$(ANTHROPIC_API_KEY) cargo run -p galoy-agents-cli

nix-run-server:
	@PG_CON=$(PG_CON) GITHUB_CLIENT_SECRET=$(GITHUB_CLIENT_SECRET) ANTHROPIC_API_KEY=$(ANTHROPIC_API_KEY) nix run .

start: reset-deps run-server
