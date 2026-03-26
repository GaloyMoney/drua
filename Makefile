PG_CON ?= postgres://user:password@localhost:5432/galoy_agents
GITHUB_CLIENT_SECRET ?= dev-secret

clean-deps:
	docker compose down -v

start-deps:
	docker compose up -d

setup-db:
	@echo "Waiting for PostgreSQL..."
	@until docker compose exec postgres pg_isready -U user -d galoy_agents > /dev/null 2>&1; do sleep 1; done
	@echo "PostgreSQL ready"
	DATABASE_URL=$(PG_CON) cargo sqlx migrate run --source domain/migrations

reset-deps: clean-deps start-deps setup-db

run-server:
	@PG_CON=$(PG_CON) GITHUB_CLIENT_SECRET=$(GITHUB_CLIENT_SECRET) nix run .

start: reset-deps run-server
