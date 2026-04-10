{
  description = "galoy-agents";

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

        galoy-agents = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
        });

        code-assistant-unwrapped = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          pname = "code-assistant";
          cargoExtraArgs = "-p code-assistant";
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

        bats-runner = pkgs.writeShellScriptBin "bats-runner" ''
          set -euo pipefail

          export TERM="''${TERM:-dumb}"
          export REPO_ROOT="$(pwd)"
          export GALOY_AGENTS_BIN="${galoy-agents}/bin/galoy-agents"
          export PG_CON="postgres://user:password@localhost:5432/galoy_agents"
          export COMPOSE_CMD="''${COMPOSE_CMD:-podman-compose-runner}"

          cleanup() {
            $COMPOSE_CMD -f "$REPO_ROOT/docker-compose.yml" down -v 2>/dev/null || true
          }
          trap cleanup EXIT

          exec bats bats/*.bats
        '';
        agentHarness = import ./images/sandbox-base/harness.nix { inherit pkgs; };
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
            cargoNextestExtraArgs = "--no-tests=pass";
          });
        };

        packages.galoy-agents-unwrapped = galoy-agents;

        packages.default = pkgs.writeShellScriptBin "galoy-agents" ''
          export ORT_DYLIB_PATH="${pkgs.onnxruntime}/lib/libonnxruntime${pkgs.stdenv.hostPlatform.extensions.sharedLibrary}"
          exec "${galoy-agents}/bin/galoy-agents" "$@"
        '';

        packages.code-assistant = pkgs.writeShellScriptBin "code-assistant" ''
          export ORT_DYLIB_PATH="${pkgs.onnxruntime}/lib/libonnxruntime${pkgs.stdenv.hostPlatform.extensions.sharedLibrary}"
          exec "${code-assistant-unwrapped}/bin/code-assistant" "$@"
        '';

        apps.bats = {
          type = "app";
          program = let
            wrapped = pkgs.writeShellScriptBin "run-bats" ''
              export PATH="${pkgs.lib.makeBinPath [
                bats-runner
                galoy-agents
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
              ]}:$PATH"
              exec bats-runner
            '';
          in "${wrapped}/bin/run-bats";
        };

        apps.harness-bats = {
          type = "app";
          program = let
            agentHarnessWrapper = pkgs.writeShellScriptBin "agent-harness" ''
              exec ${pkgs.nodejs_22}/bin/node ${agentHarness}/lib/index.js "$@"
            '';
            wrapped = pkgs.writeShellScriptBin "run-harness-bats" ''
              set -euo pipefail
              export TERM="''${TERM:-dumb}"
              export AGENT_HARNESS_BIN="${agentHarnessWrapper}/bin/agent-harness"
              export PATH="${pkgs.lib.makeBinPath [
                pkgs.bats
                pkgs.jq
                pkgs.curl
                pkgs.coreutils
                pkgs.gawk
                pkgs.gnugrep
                pkgs.nodejs_22
                pkgs.git
                pkgs.cacert
                pkgs.shadow
              ]}:$PATH"

              # Claude Code CLI refuses --dangerously-skip-permissions as root.
              # Drop to a non-root user when running in CI.
              if [ "$(id -u)" = "0" ]; then
                useradd -m testuser 2>/dev/null || true
                exec su testuser -s /bin/sh -c "
                  ANTHROPIC_API_KEY=$ANTHROPIC_API_KEY \
                  AGENT_HARNESS_BIN=$AGENT_HARNESS_BIN \
                  TERM=$TERM \
                  PATH=$PATH \
                  HOME=/home/testuser \
                  bats bats/harness.bats
                "
              fi

              exec bats bats/harness.bats
            '';
          in "${wrapped}/bin/run-harness-bats";
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
          name = "galoy-agents";
          tag = "latest";
          contents = [
            galoy-agents
            pkgs.cacert
            pkgs.onnxruntime
          ];
          config = {
            Cmd = [ "${galoy-agents}/bin/galoy-agents" ];
            Env = [
              "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              "ORT_DYLIB_PATH=${pkgs.onnxruntime}/lib/libonnxruntime${pkgs.stdenv.hostPlatform.extensions.sharedLibrary}"
            ];
          };
        };

        packages.sandbox-base-image = import ./images/sandbox-base/default.nix { inherit pkgs; };

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
            echo "galoy-agents dev shell loaded"
          '';
        };
      }
    );
}
