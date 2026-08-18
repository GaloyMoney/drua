PG_CON ?= postgres://user:password@localhost:5432/drua
GITHUB_CLIENT_SECRET ?= $(shell echo $$GITHUB_CLIENT_SECRET)
ANTHROPIC_API_KEY ?= $(shell echo $$ANTHROPIC_API_KEY)

# ── Dev dependencies — native via nix, no container engine ──────────────────────
# Postgres+pgvector run as local processes (see packages.pg-start in
# flake.nix). No docker/podman, no VM, no Rosetta on apple silicon.
start-deps:
	nix run .#pg-start

stop-deps:
	nix run .#pg-stop

clean-deps: stop-deps
	@rm -rf .nix-deps/pg .nix-deps/pg.log

# OTLP collector (dev/otel-agent-config.yaml): OTLP on :4317/:4318,
# forwards to Honeycomb when INGEST_HONEYCOMB_API_KEY is set. Runs in
# the foreground; Ctrl-C stops it.
start-otel:
	nix run .#otel-agent

setup-db:
	@echo "Waiting for PostgreSQL..."
	@until pg_isready -h localhost -p 5432 -U user -d drua > /dev/null 2>&1; do sleep 1; done
	@echo "PostgreSQL ready"
	DATABASE_URL=$(PG_CON) cargo sqlx migrate run --source core/migrations

reset-deps: clean-deps start-deps setup-db

sqlx-prepare:
	DATABASE_URL=$(PG_CON) cargo sqlx prepare --workspace -- --all-targets

build-sandbox:
	cargo build -p sandbox-tool-server

run-server: build-sandbox
	@PG_CON=$(PG_CON) GITHUB_CLIENT_SECRET=$(GITHUB_CLIENT_SECRET) ANTHROPIC_API_KEY=$(ANTHROPIC_API_KEY) cargo run -p drua-cli -- server $(ARGS)

nix-run-server:
	@PG_CON=$(PG_CON) GITHUB_CLIENT_SECRET=$(GITHUB_CLIENT_SECRET) ANTHROPIC_API_KEY=$(ANTHROPIC_API_KEY) nix run . -- server $(ARGS)

sdl-rust:
	SQLX_OFFLINE=true cargo run --bin write_sdl > server/src/graphql/schema.graphql

generate-default-config:
	SQLX_OFFLINE=true cargo run -q -p drua-cli -- server dump-default-config > dev/drua.default.yml

integration-tests: reset-deps
	DATABASE_URL=$(PG_CON) cargo nextest run

# Regenerate bats snapshots under bats/summarized-tool-responses/ from
# the live gateway. Sets UPDATE_FIXTURES=1 so the bats assertions write
# fresh files instead of diffing. Rebuilds release binaries first so
# bats doesn't spawn `cargo run` (too slow for the 15s readiness wait).
update-fixtures:
	DATABASE_URL=$(PG_CON) cargo build --release -p drua-cli -p fake-mcp-upstream
	$(MAKE) reset-deps
	SKIP_DEPS=1 UPDATE_FIXTURES=1 \
		DRUA_BIN=$(PWD)/target/release/drua \
		FAKE_UPSTREAM_BIN=$(PWD)/target/release/fake-mcp-upstream \
		PG_CON=$(PG_CON) \
		bats -t bats/fake_mcp_upstream.bats

start: reset-deps
	@PG_CON=$(PG_CON) ANTHROPIC_API_KEY=$(ANTHROPIC_API_KEY) cargo run -p drua-cli -- server --set oauth.login=dev $(ARGS)

# ── Test library ──────────────────────────────────────────────────────────────
TEST_LIBRARY_REPO ?= git@github.com:galoymoney/drua-test-library.git
TEST_LIBRARY_DIR  = tmp/drua-test-library

reset-test-library:
	rm -rf $(TEST_LIBRARY_DIR) .library
	mkdir -p $(TEST_LIBRARY_DIR)
	cd $(TEST_LIBRARY_DIR) && git init && git remote add origin $(TEST_LIBRARY_REPO)
	mkdir -p $(TEST_LIBRARY_DIR)/runtime/skills
	mkdir -p $(TEST_LIBRARY_DIR)/runtime/projects
	touch $(TEST_LIBRARY_DIR)/runtime/skills/.gitkeep
	touch $(TEST_LIBRARY_DIR)/runtime/projects/.gitkeep
	cd $(TEST_LIBRARY_DIR) && git add -A && \
		git -c user.name=drua -c user.email=drua@galoy.io commit -m "init: empty library scaffold" && \
		git push --force origin HEAD:main
	@echo "Test library reset to empty scaffold. Restart the server to re-clone .library."

add-test-skill:
	@test -d $(TEST_LIBRARY_DIR)/.git || (echo "Run 'make reset-test-library' first" && exit 1)
	@printf '# CI Check\n\nInvestigate the latest CI status for a Concourse pipeline.\n\n---\n\nUsing the concourse tools, find the most recent build failure of the **galoy-agents-bin** pipeline.\n\n1. List recent builds and identify the last failed one\n2. Fetch the build logs and summarize the failure reason\n3. If all recent builds passed, report that the pipeline is green\n\nIf $$ARGUMENTS is provided, check that pipeline instead.\n' \
		> $(TEST_LIBRARY_DIR)/runtime/skills/ci-check.md
	cd $(TEST_LIBRARY_DIR) && git pull --rebase origin main && \
		git add -A && \
		git -c user.name=drua -c user.email=drua@galoy.io commit -m "add global skill: ci-check" && \
		git push origin HEAD:main
	@echo "Pushed ci-check skill to test library."
