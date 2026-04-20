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
            craneLib.filterCargoSources path type;
        };

        commonArgs = {
          inherit src;
          strictDeps = true;
          SQLX_OFFLINE = true;
          nativeBuildInputs = [
            pkgs.pkg-config
          ];
          buildInputs = pkgs.lib.optionals pkgs.stdenv.isDarwin [
            pkgs.libiconv
          ];
        };

        fly = pkgs.stdenv.mkDerivation rec {
          pname = "fly";
          version = "8.0.1";
          src = pkgs.fetchurl {
            url = "https://github.com/concourse/concourse/releases/download/v${version}/fly-${version}-${
              if pkgs.stdenv.isDarwin then "darwin" else "linux"
            }-${
              if pkgs.stdenv.hostPlatform.isAarch64 then "arm64" else "amd64"
            }.tgz";
            sha256 =
              if pkgs.stdenv.isDarwin && pkgs.stdenv.hostPlatform.isAarch64 then "sha256-eXF29GNUby57Q6nE4aHfzi1FikFlksnaOuiEWICzd2Y="
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
          cargoExtraArgs = "-p drua-web --bin write_sdl";
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
            --no-fail-fast \
            2>&1
        '';

        bats-runner = pkgs.writeShellScriptBin "bats-runner" ''
          set -euo pipefail

          export TERM="''${TERM:-dumb}"
          export REPO_ROOT="$(pwd)"
          export DRUA_BIN="${drua}/bin/drua"
          # Point bats/sandbox-helpers.bash at the nix-built binary so
          # setup_file doesn't fall back to `cargo run` (which fetches
          # + compiles the crate in CI and blows past the bats timeout).
          export SANDBOX_TOOL_SERVER_BIN="${sandboxToolServerBin}/bin/sandbox-tool-server"
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
              if ! diff -u web/src/graphql/schema.graphql schema-generated.graphql; then
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
              ${drua}/bin/drua dump-default-config > default-config-generated.yml

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
              export PATH="${pkgs.lib.makeBinPath [
                bats-runner
                drua
                podmanPkgs.podman-compose-runner
                pkgs.bats
                pkgs.jq
                pkgs.curl
                pkgs.coreutils
                pkgs.gawk
                pkgs.gnugrep
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
          ];
          config = {
            Cmd = [ "${drua}/bin/drua" ];
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
            pkgs.docker-compose
            pkgs.podman
            pkgs.podman-compose
            pkgs.opentofu
            pkgs.ytt
            pkgs.kubernetes-helm
            pkgs.minikube
            pkgs.kubectl
            pkgs.vendir
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

            echo "drua dev shell loaded (engine: $ENGINE_DEFAULT)"
          '';
        };
      }
    );
}
