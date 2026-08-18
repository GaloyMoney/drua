{
  description = "drua";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, crane }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" "rustfmt" "clippy" ];
        };

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;
        src = pkgs.lib.cleanSourceWith {
          src = craneLib.path ./.;
          filter = path: type:
            (builtins.match ".*\.sqlx/.*" path != null) ||
            (builtins.match ".*\.sql$" path != null) ||
            (builtins.match ".*\.html$" path != null) ||
            (builtins.match ".*\.yml$" path != null) ||
            (builtins.match ".*\.bash$" path != null) ||
            (builtins.match ".*\.bats$" path != null) ||
            (builtins.match ".*\.toml$" path != null) ||
            (builtins.match ".*\.jsonl$" path != null) ||
            (builtins.match ".*\.graphql$" path != null) ||
            # Test fixtures with arbitrary extensions — `.log` files
            # captured from upstream tools (concourse, etc.) used by
            # `include_str!` in integration tests. Keep them inside
            # `tests/fixtures/` so this carve-out stays narrow.
            (builtins.match ".*/tests/fixtures/.*" path != null) ||
            craneLib.filterCargoSources path type;
        };

        commonArgs = {
          inherit src;
          strictDeps = true;
          SQLX_OFFLINE = true;
          nativeBuildInputs = [
            pkgs.pkg-config
            # `git2`'s `vendored-openssl` feature (pulled in by
            # drua-library) builds OpenSSL from source via
            # `openssl-src`, which shells out to perl during
            # ./Configure.
            pkgs.perl
          ];
          buildInputs = pkgs.lib.optionals pkgs.stdenv.isDarwin [
            pkgs.libiconv
          ];
        };

        fly = pkgs.stdenv.mkDerivation rec {
          pname = "fly";
          version = "8.1.1";
          src = pkgs.fetchurl {
            url = "https://github.com/concourse/concourse/releases/download/v${version}/fly-${version}-${
              if pkgs.stdenv.isDarwin then "darwin" else "linux"
            }-${
              if pkgs.stdenv.hostPlatform.isAarch64 then "arm64" else "amd64"
            }.tgz";
            sha256 =
              if pkgs.stdenv.isDarwin && pkgs.stdenv.hostPlatform.isAarch64 then "sha256-MUK+FxacT6p+H/apfgsaWxp/cBLmUkeltswSQl9PC78="
              else if pkgs.stdenv.isDarwin then "sha256-PLACEHOLDER-darwin-amd64"
              else if pkgs.stdenv.hostPlatform.isAarch64 then "sha256-PLACEHOLDER-linux-arm64"
              else "sha256-PLACEHOLDER-linux-amd64";
          };
          phases = [ "unpackPhase" "installPhase" ];
          unpackPhase = "tar -xzf $src";
          installPhase = ''
            mkdir -p $out/bin
            cp fly $out/bin/
            chmod +x $out/bin/fly
          '';
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        drua = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          doCheck = false;
        });

        code-assistant-unwrapped = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          pname = "code-assistant";
          cargoExtraArgs = "-p code-assistant";
        });

        write-sdl = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          pname = "write-sdl";
          cargoExtraArgs = "-p drua-server --bin write_sdl";
        });

        fake-mcp-upstream = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          pname = "fake-mcp-upstream";
          cargoExtraArgs = "-p fake-mcp-upstream";
          doCheck = false;
        });

        pythonEnv = pkgs.python3.withPackages (ps:
          with ps; [
            torch
            sentence-transformers
            transformers
            tokenizers
            scikit-learn
            onnx
            onnxruntime
            joblib
            numpy
            einops
            datasets
            accelerate
            onnxscript
          ]);

        podmanPkgs = import ./nix/podman-runner.nix {
          inherit pkgs;
          inherit (pkgs) lib stdenv;
        };

        sandboxToolServerBin = pkgs.writeShellScriptBin "sandbox-tool-server" ''
          exec ${self.packages.${system}.sandbox-tool-server}/bin/sandbox-tool-server "$@"
        '';

        integration-test-archive = craneLib.mkCargoDerivation (commonArgs // {
          inherit cargoArtifacts;
          pnameSuffix = "-nextest-archive";
          nativeBuildInputs = commonArgs.nativeBuildInputs ++ [
            pkgs.cargo-nextest
          ];
          buildPhaseCargoCommand = ''
            cargo nextest archive --archive-file test-archive.tar.zst
          '';
          installPhaseCommand = ''
            mkdir -p $out
            cp test-archive.tar.zst $out/
          '';
        });

        integration-test-runner = pkgs.writeShellScriptBin "integration-test-runner" ''
          set -euo pipefail

          export TERM="''${TERM:-dumb}"
          export REPO_ROOT="$(pwd)"
          export PG_CON="postgres://user:password@localhost:5432/drua"
          export DATABASE_URL="$PG_CON"
          export ORT_DYLIB_PATH="${pkgs.onnxruntime}/lib/libonnxruntime${pkgs.stdenv.hostPlatform.extensions.sharedLibrary}"
          export COMPOSE_CMD="''${COMPOSE_CMD:-podman-compose-runner}"

          cleanup() {
            $COMPOSE_CMD -f "$REPO_ROOT/docker-compose.yml" down -v 2>/dev/null || true
          }
          trap cleanup EXIT

          $COMPOSE_CMD -f "$REPO_ROOT/docker-compose.yml" up -d postgres

          echo "Waiting for PostgreSQL..."
          for i in $(seq 1 30); do
            if pg_isready -h localhost -p 5432 -U user -d drua >/dev/null 2>&1; then
              echo "PostgreSQL ready"
              break
            fi
            if [ "$i" = "30" ]; then
              echo "ERROR: PostgreSQL failed to start within 30s"
              exit 1
            fi
            sleep 1
          done

          sqlx migrate run --source "$REPO_ROOT/core/migrations"

          cargo-nextest nextest run \
            --archive-file ${integration-test-archive}/test-archive.tar.zst \
            --workspace-remap "$REPO_ROOT" \
            --test-threads 1 \
            --failure-output immediate-final \
            --color never \
            -E 'not binary(sandbox-tool-server)' \
            2>&1 | cat
        '';

        bats-runner = pkgs.writeShellScriptBin "bats-runner" ''
          set -euo pipefail

          export TERM="''${TERM:-dumb}"
          export REPO_ROOT="$(pwd)"
          export DRUA_BIN="${drua}/bin/drua"
          export FAKE_UPSTREAM_BIN="${fake-mcp-upstream}/bin/fake-mcp-upstream"
          export ORT_DYLIB_PATH="${pkgs.onnxruntime}/lib/libonnxruntime${pkgs.stdenv.hostPlatform.extensions.sharedLibrary}"
          # Point bats/sandbox-helpers.bash at the nix-built binary so
          # setup_file doesn't fall back to `cargo run` (which fetches
          # + compiles the crate in CI and blows past the bats timeout).
          export SANDBOX_TOOL_SERVER_BIN="${sandboxToolServerBin}/bin/sandbox-tool-server"
          export TUNNEL_FIXTURE_BIN="${self.packages.${system}.tunnel-fixture}/bin/tunnel-fixture"
          export PG_CON="postgres://user:password@localhost:5432/drua"
          export COMPOSE_CMD="''${COMPOSE_CMD:-podman-compose-runner}"

          cleanup() {
            $COMPOSE_CMD -f "$REPO_ROOT/docker-compose.yml" down -v 2>/dev/null || true
          }
          trap cleanup EXIT

          exec bats bats/*.bats
        '';
      in
      {
        checks = {
          fmt = craneLib.cargoFmt { inherit src; };

          clippy = craneLib.cargoClippy (commonArgs // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- --deny warnings";
          });

          nextest = craneLib.cargoNextest (commonArgs // {
            inherit cargoArtifacts;
            cargoNextestExtraArgs = "--lib";
          });

          graphql-schema = pkgs.stdenv.mkDerivation {
            name = "graphql-schema-check";
            src = src;
            nativeBuildInputs = [ pkgs.diffutils ];
            buildInputs = [ write-sdl ];
            buildPhase = ''
              echo "Generating GraphQL SDL..."
              ${write-sdl}/bin/write_sdl > schema-generated.graphql

              echo "Comparing with committed schema..."
              if ! diff -u server/src/graphql/schema.graphql schema-generated.graphql; then
                echo "ERROR: GraphQL schema is out of date!"
                echo "Run 'make sdl-rust' to update the schema"
                exit 1
              fi

              echo "GraphQL schema is up to date"
            '';
            installPhase = ''
              mkdir -p $out
              echo "GraphQL schema check passed" > $out/result.txt
            '';
          };

          default-config = pkgs.stdenv.mkDerivation {
            name = "default-config-check";
            src = src;
            nativeBuildInputs = [ pkgs.diffutils ];
            buildInputs = [ drua ];
            buildPhase = ''
              echo "Generating default config..."
              ${drua}/bin/drua server dump-default-config > default-config-generated.yml

              echo "Comparing with committed default config..."
              if ! diff -u dev/drua.default.yml default-config-generated.yml; then
                echo "ERROR: Default config is out of date!"
                echo "Run 'make generate-default-config' to update the config"
                exit 1
              fi

              echo "Default config is up to date"
            '';
            installPhase = ''
              mkdir -p $out
              echo "Default config check passed" > $out/result.txt
            '';
          };
        };

        packages.drua-unwrapped = drua;

        packages.default = pkgs.writeShellScriptBin "drua" ''
          export ORT_DYLIB_PATH="${pkgs.onnxruntime}/lib/libonnxruntime${pkgs.stdenv.hostPlatform.extensions.sharedLibrary}"
          exec "${drua}/bin/drua" "$@"
        '';

        packages.code-assistant = pkgs.writeShellScriptBin "code-assistant" ''
          export ORT_DYLIB_PATH="${pkgs.onnxruntime}/lib/libonnxruntime${pkgs.stdenv.hostPlatform.extensions.sharedLibrary}"
          exec "${code-assistant-unwrapped}/bin/code-assistant" "$@"
        '';

        apps.bats = {
          type = "app";
          program = let
            wrapped = pkgs.writeShellScriptBin "run-bats" ''
              # ripgrep is required by the sandbox server's Grep / Glob
              # handlers — bats/sandbox.bats spawns sandbox-tool-server
              # via this PATH so `rg` has to be present here too (the
              # production sandbox image bakes it in separately).
              # `diffutils` provides `diff`, used by
              # bats/fake_mcp_upstream.bats to compare snapshot fixtures
              # under bats/summarized-tool-responses/.
              export PATH="${pkgs.lib.makeBinPath [
                bats-runner
                drua
                podmanPkgs.podman-compose-runner
                pkgs.bats
                pkgs.diffutils
                pkgs.jq
                pkgs.curl
                pkgs.coreutils
                pkgs.gawk
                pkgs.gnugrep
                pkgs.gnused
                pkgs.postgresql
                pkgs.procps
                pkgs.util-linux
                pkgs.git
                pkgs.ripgrep
              ]}:$PATH"
              exec bats-runner
            '';
          in "${wrapped}/bin/run-bats";
        };

        apps.sandbox-bats = {
          type = "app";
          program = let
            sandboxToolServerWrapper = pkgs.writeShellScriptBin "sandbox-tool-server" ''
              exec ${self.packages.${system}.sandbox-tool-server}/bin/sandbox-tool-server "$@"
            '';
            wrapped = pkgs.writeShellScriptBin "run-sandbox-bats" ''
              set -euo pipefail
              export TERM="''${TERM:-dumb}"
              export SANDBOX_TOOL_SERVER_BIN="${sandboxToolServerWrapper}/bin/sandbox-tool-server"
              # ripgrep is required by the Grep / Glob server handlers —
              # the sandbox image bakes it in, but the bats runner spawns
              # the binary out-of-image so we have to add it explicitly.
              export PATH="${pkgs.lib.makeBinPath [
                pkgs.bats
                pkgs.jq
                pkgs.curl
                pkgs.coreutils
                pkgs.gawk
                pkgs.gnugrep
                pkgs.git
                pkgs.ripgrep
              ]}:$PATH"

              exec bats bats/sandbox.bats
            '';
          in "${wrapped}/bin/run-sandbox-bats";
        };

        apps.integration-tests = {
          type = "app";
          program = let
            wrapped = pkgs.writeShellScriptBin "run-integration-tests" ''
              export PATH="${pkgs.lib.makeBinPath [
                integration-test-runner
                podmanPkgs.podman-compose-runner
                pkgs.cargo-nextest
                pkgs.sqlx-cli
                pkgs.postgresql
                pkgs.coreutils
              ]}:$PATH"
              exec integration-test-runner
            '';
          in "${wrapped}/bin/run-integration-tests";
        };

        apps.prep-code-assistant = {
          type = "app";
          program = let
            prep = pkgs.writeShellScriptBin "prep-code-assistant" ''
              set -euo pipefail

              export ORT_DYLIB_PATH="${pkgs.onnxruntime}/lib/libonnxruntime${pkgs.stdenv.hostPlatform.extensions.sharedLibrary}"

              CODE_ASSISTANT_DIR="$(pwd)/code-assistant"
              ONNX_DIR="$CODE_ASSISTANT_DIR/models/onnx"
              export CODE_ASSISTANT_CONFIG="$CODE_ASSISTANT_DIR/config.toml"

              # --- Step 1: train model if missing ---
              if [ ! -f "$ONNX_DIR/model.onnx" ]; then
                echo "=== ONNX model not found — training from labels ==="
                cd "$CODE_ASSISTANT_DIR"
                ${pythonEnv}/bin/python3 scripts/train_setfit.py
                ${pythonEnv}/bin/python3 scripts/export_onnx.py
                cd - > /dev/null
              else
                echo "=== ONNX model found at $ONNX_DIR ==="
              fi

              # --- Step 2: bootstrap (clone/update repos + index) ---
              echo ""
              echo "=== Bootstrapping code-assistant ==="
              "${code-assistant-unwrapped}/bin/code-assistant" bootstrap

              # --- Step 3: apply labels ---
              echo ""
              echo "=== Applying heuristic labels ==="
              "${code-assistant-unwrapped}/bin/code-assistant" label

              echo ""
              echo "=== Replaying human-reviewed labels ==="
              "${code-assistant-unwrapped}/bin/code-assistant" replay-labels

              echo ""
              echo "=== Done! Run 'make start' in code-assistant/ to launch. ==="
            '';
          in "${prep}/bin/prep-code-assistant";
        };

        packages.docker-image = pkgs.dockerTools.buildLayeredImage {
          name = "drua";
          tag = "latest";
          contents = [
            drua
            pkgs.cacert
            pkgs.onnxruntime
            pkgs.git
          ];
          config = {
            Cmd = [ "${drua}/bin/drua" "server" ];
            Env = [
              "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              "ORT_DYLIB_PATH=${pkgs.onnxruntime}/lib/libonnxruntime${pkgs.stdenv.hostPlatform.extensions.sharedLibrary}"
            ];
          };
        };

        packages.sandbox-tool-server = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          pname = "sandbox-tool-server";
          cargoExtraArgs = "-p sandbox-tool-server";
        });

        packages.sandbox-image = import ./images/sandbox/default.nix {
          inherit pkgs;
          sandbox-tool-server = self.packages.${system}.sandbox-tool-server;
        };

        packages.tunnel-connector = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          pname = "tunnel-connector";
          cargoExtraArgs = "-p tunnel-connector --bin tunnel-connector";
        });

        packages.tunnel-fixture = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          pname = "tunnel-fixture";
          cargoExtraArgs = "-p tunnel-connector --bin tunnel-fixture";
        });

        packages.tunnel-connector-image = import ./images/tunnel-connector/default.nix {
          inherit pkgs;
          tunnel-connector = self.packages.${system}.tunnel-connector;
        };

        packages.concourse-drua-resource-image = import ./images/concourse-drua-resource/default.nix {
          inherit pkgs;
        };

        # Native PostgreSQL + pgvector for local dev/tests — no container
        # VM (and on apple silicon, none of the podman machine's Rosetta
        # overhead). Matches the compose stack's pgvector/pgvector:pg16
        # image: user `user`, db `drua`, trust auth, port 5432 — i.e. the
        # Makefile's PG_CON default.
        packages.pg-start = let
          pg = pkgs.postgresql_17.withPackages (p: [p.pgvector]);
        in
          pkgs.writeShellApplication {
            name = "pg-start";
            runtimeInputs = [pkgs.coreutils];
            text = ''
              set -euo pipefail
              NAME=pg
              PORT=5432
              PGUSER=user
              DB=drua
              PGDATA="$PWD/.nix-deps/$NAME"
              LOG="$PWD/.nix-deps/$NAME.log"

              mkdir -p "$PWD/.nix-deps"

              if [ ! -f "$PGDATA/PG_VERSION" ]; then
                echo "[$NAME] Initializing data directory at $PGDATA..."
                ${pg}/bin/initdb -D "$PGDATA" --username="$PGUSER" --auth=trust --no-locale -E UTF8
                {
                  echo "port = $PORT"
                  echo "max_connections = 200"
                  echo "unix_socket_directories = '/tmp'"
                  echo "listen_addresses = '127.0.0.1'"
                } >> "$PGDATA/postgresql.conf"
              fi

              if ${pg}/bin/pg_ctl -D "$PGDATA" status >/dev/null 2>&1; then
                echo "[$NAME] Already running on port $PORT"
              else
                # Stale pid file from an unclean shutdown blocks start.
                if [ -f "$PGDATA/postmaster.pid" ]; then
                  rm -f "$PGDATA/postmaster.pid"
                fi
                # Every binary must be invoked via its absolute store
                # path: the withPlugins `postgres` wrapper resolves
                # $libdir from argv0, so a relative invocation breaks all
                # extension loads ("could not access file $libdir/vector").
                ${pg}/bin/pg_ctl -D "$PGDATA" -l "$LOG" -w start
              fi

              ${pg}/bin/pg_isready -h 127.0.0.1 -p "$PORT" -U "$PGUSER" -q

              ${pg}/bin/createdb -h 127.0.0.1 -p "$PORT" -U "$PGUSER" "$DB" 2>/dev/null \
                || echo "[$NAME] Database '$DB' already exists"

              ${pg}/bin/psql -h 127.0.0.1 -p "$PORT" -U "$PGUSER" -d "$DB" \
                -c 'CREATE EXTENSION IF NOT EXISTS vector;'

              echo "[$NAME] Ready: postgres://$PGUSER@127.0.0.1:$PORT/$DB"
              echo "[$NAME] Migrations: make setup-db   Stop: make stop-deps"
            '';
          };

        # Companion to pg-start. Self-contained (uses the same nix-built
        # pg_ctl via absolute store path) and refuses to mask failures:
        # a server that is running but cannot be stopped is an error, so
        # callers like clean-deps never rm -rf a live data directory.
        packages.pg-stop = let
          pg = pkgs.postgresql_17.withPackages (p: [p.pgvector]);
        in
          pkgs.writeShellApplication {
            name = "pg-stop";
            runtimeInputs = [pkgs.coreutils];
            text = ''
              set -euo pipefail
              NAME=pg
              PGDATA="$PWD/.nix-deps/$NAME"

              if [ ! -f "$PGDATA/postmaster.pid" ]; then
                echo "[$NAME] Not running"
                exit 0
              fi

              if ! ${pg}/bin/pg_ctl -D "$PGDATA" status >/dev/null 2>&1; then
                # Stale pid file — no live postmaster.
                echo "[$NAME] Not running (stale pid file)"
                rm -f "$PGDATA/postmaster.pid"
                exit 0
              fi

              ${pg}/bin/pg_ctl -D "$PGDATA" stop -m fast -w
              echo "[$NAME] Stopped"
            '';
          };

        devShells.training = pkgs.mkShell {
          buildInputs = [ pythonEnv ];
          shellHook = ''
            echo "code-assistant training shell loaded (Python $(python3 --version 2>&1 | cut -d' ' -f2))"
          '';
        };

        devShells.default = pkgs.mkShell {
          buildInputs = [
            rustToolchain
            pkgs.bats
            pkgs.bacon
            pkgs.cargo-nextest
            pkgs.sqlx-cli
            pkgs.postgresql
            pkgs.pkg-config
            # `git2`'s `vendored-openssl` (drua-library) builds OpenSSL
            # via `openssl-src`, which shells out to perl during
            # ./Configure. Without perl the libgit2-sys build silently
            # falls back to a libgit2 with no HTTPS transport, and
            # https:// clones fail at runtime with "unsupported URL
            # protocol" (libgit2 error class=Net 12).
            pkgs.perl
            pkgs.docker-compose
            pkgs.podman
            pkgs.podman-compose
            pkgs.opentofu
            pkgs.ytt
            pkgs.kubernetes-helm
            pkgs.minikube
            pkgs.kubectl
            pkgs.vendir
            pkgs.sd
            pkgs.fd
            pkgs.ripgrep
            pkgs.gnused
          ] ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            fly
            pkgs.libiconv
          ];

          shellHook = ''
            # Container engine auto-detection
            unset DOCKER_HOST
            if [[ -n "''${ENGINE_DEFAULT:-}" ]]; then
              :
            elif command -v podman &>/dev/null && ! command -v docker &>/dev/null; then
              export ENGINE_DEFAULT=podman
            else
              export ENGINE_DEFAULT=docker
            fi

            export ORT_DYLIB_PATH="${pkgs.onnxruntime}/lib/libonnxruntime${pkgs.stdenv.hostPlatform.extensions.sharedLibrary}"

            # Local-dev: route the in-cluster sandbox image's git ops at
            # the dev-mode drua-server's git-proxy. LocalAdminClient
            # picks these up at sandbox-spawn time and renders a per-
            # sandbox `gitconfig` with `insteadOf` + a Bearer token.
            # Production sandboxes (K8s) ignore both — the projected
            # SA token + chart's `sandbox.gitProxyUrl` cover that path.
            #
            # The dev token is the literal string `dev-agent` — when
            # `oauth.dev_mode_agent_tokens=true` in `drua.yml` the
            # auth middleware accepts it and synthesises an
            # `AuthSubject::Agent` with nil project_id + agent_id (no
            # DB lookup required). Audit rows mark dev traffic with
            # nil UUIDs so it's grep-able after the fact.
            : "''${DRUA_GIT_PROXY_URL:=http://localhost:4200/git}"
            : "''${DRUA_DEV_AGENT_TOKEN:=dev-agent}"
            export DRUA_GIT_PROXY_URL DRUA_DEV_AGENT_TOKEN

            echo "drua dev shell loaded (engine: $ENGINE_DEFAULT)"
          '';
        };
      }
    );
}
