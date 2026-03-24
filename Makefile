clean-deps:
	docker compose down -v

start-deps:
	docker compose up -d

setup-db:
	@echo "Waiting for PostgreSQL..."
	@until docker compose exec postgres pg_isready -U user -d galoy_agents > /dev/null 2>&1; do sleep 1; done
	@echo "PostgreSQL ready"
	DATABASE_URL=${PG_CON} cargo sqlx migrate run --source domain/migrations

reset-deps: clean-deps start-deps setup-db

run-server:
	cargo run --bin galoy-agents

start: reset-deps run-server
