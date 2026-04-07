{
  description = "galoy-agents CI scripts";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    parent.url = "path:..";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    parent,
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = nixpkgs.legacyPackages.${system};
      code-assistant = parent.packages.${system}.code-assistant;

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
    in {
      formatter = pkgs.alejandra;

      packages.train-classifier = pkgs.writeShellScriptBin "train-classifier" ''
        set -euo pipefail

        OUTPUT_DIR="''${1:?Usage: train-classifier <output-dir>}"

        export PATH="${pkgs.lib.makeBinPath [ pythonEnv pkgs.coreutils ]}:$PATH"

        cd code-assistant

        echo "=== Training SetFit classifier ==="
        python3 scripts/train_setfit.py

        echo "=== Exporting to ONNX ==="
        python3 scripts/export_onnx.py

        mkdir -p "$OUTPUT_DIR"
        HASH=$(sha256sum models/onnx/model.onnx | cut -d' ' -f1)
        tar -czf "$OUTPUT_DIR/$HASH.tar.gz" -C models onnx
        echo "Model hash: $HASH"
      '';

      packages.build-code-assistant-index = pkgs.writeShellScriptBin "build-code-assistant-index" ''
        set -euo pipefail

        REPOS_DIR="''${1:?Usage: build-code-assistant-index <repos-dir> <index-output-dir> <embedder-output-dir> [model-dir]}"
        OUTPUT_DIR="''${2:?Usage: build-code-assistant-index <repos-dir> <index-output-dir> <embedder-output-dir> [model-dir]}"
        EMBEDDER_DIR="''${3:?Usage: build-code-assistant-index <repos-dir> <index-output-dir> <embedder-output-dir> [model-dir]}"
        MODEL_DIR="''${4:-}"

        export CODE_ASSISTANT_CONFIG="$(pwd)/ci/config.code-assistant.toml"
        export PATH="${pkgs.lib.makeBinPath [ code-assistant pkgs.coreutils pkgs.findutils pkgs.gnutar pkgs.gzip ]}:$PATH"

        cd code-assistant

        # If a model directory was provided, extract it
        if [ -n "$MODEL_DIR" ] && [ -d "$MODEL_DIR" ]; then
          MODEL_TAR=$(ls "$MODEL_DIR"/*.tar.gz 2>/dev/null | head -1)
          if [ -n "$MODEL_TAR" ]; then
            echo "Extracting ONNX model from $MODEL_TAR"
            mkdir -p ./data/models
            tar -xzf "$MODEL_TAR" -C ./data/models
          fi
        fi

        code-assistant build-index --repos-dir "$REPOS_DIR"

        mkdir -p "$OUTPUT_DIR"
        HASH=$(sha256sum ./data/code-assistant.db | cut -d' ' -f1)
        tar -czf "$OUTPUT_DIR/$HASH.tar.gz" -C ./data code-assistant.db
        echo "Index hash: $HASH"

        # Package fastembed model cache for init container
        if [ -d .fastembed_cache ]; then
          mkdir -p "$EMBEDDER_DIR"
          EMBED_HASH=$(find .fastembed_cache -type f -exec sha256sum {} + | sort | sha256sum | cut -d' ' -f1)
          tar -czf "$EMBEDDER_DIR/$EMBED_HASH.tar.gz" .fastembed_cache
          echo "Embedder hash: $EMBED_HASH"
        fi
      '';

      packages.commit-hash = pkgs.writeShellScriptBin "commit-hash" ''
        set -euo pipefail

        REPO_DIR="''${1:?Usage: commit-hash <repo-dir> <values-path> <yaml-path> <hash>}"
        VALUES_FILE="''${2:?Usage: commit-hash <repo-dir> <values-path> <yaml-path> <hash>}"
        YAML_PATH="''${3:?Usage: commit-hash <repo-dir> <values-path> <yaml-path> <hash>}"
        HASH="''${4:?Usage: commit-hash <repo-dir> <values-path> <yaml-path> <hash>}"

        export PATH="${pkgs.lib.makeBinPath [
          pkgs.git
          pkgs.yq-go
        ]}:$PATH"

        cd "$REPO_DIR"

        git config user.email "bot@galoy.io"
        git config user.name "CI Bot"

        yq -i ".$YAML_PATH = \"$HASH\"" "$VALUES_FILE"

        if git diff --quiet "$VALUES_FILE"; then
          echo "No change to $YAML_PATH, skipping commit"
          exit 0
        fi

        git add "$VALUES_FILE"
        git commit -m "ci(chart): update $YAML_PATH to $HASH"
      '';
    });
}
