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
      style-agent = parent.packages.${system}.style-agent;

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

        cd style-agent

        echo "=== Training SetFit classifier ==="
        python3 scripts/train_setfit.py

        echo "=== Exporting to ONNX ==="
        python3 scripts/export_onnx.py

        mkdir -p "$OUTPUT_DIR"
        HASH=$(sha256sum models/onnx/model.onnx | cut -d' ' -f1)
        tar -czf "$OUTPUT_DIR/$HASH.tar.gz" -C models onnx
        echo "Model hash: $HASH"
      '';

      packages.build-style-index = pkgs.writeShellScriptBin "build-style-index" ''
        set -euo pipefail

        REPOS_DIR="''${1:?Usage: build-style-index <repos-dir> <index-output-dir> <embedder-output-dir> [model-dir]}"
        OUTPUT_DIR="''${2:?Usage: build-style-index <repos-dir> <index-output-dir> <embedder-output-dir> [model-dir]}"
        EMBEDDER_DIR="''${3:?Usage: build-style-index <repos-dir> <index-output-dir> <embedder-output-dir> [model-dir]}"
        MODEL_DIR="''${4:-}"

        export STYLE_AGENT_CONFIG="$(pwd)/ci/config.style-agent.toml"
        export PATH="${pkgs.lib.makeBinPath [ style-agent ]}:$PATH"

        cd style-agent

        # If a model directory was provided, extract it
        if [ -n "$MODEL_DIR" ] && [ -d "$MODEL_DIR" ]; then
          MODEL_TAR=$(ls "$MODEL_DIR"/*.tar.gz 2>/dev/null | head -1)
          if [ -n "$MODEL_TAR" ]; then
            echo "Extracting ONNX model from $MODEL_TAR"
            mkdir -p ./data/models
            tar -xzf "$MODEL_TAR" -C ./data/models
          fi
        fi

        style-agent build-index --repos-dir "$REPOS_DIR"

        mkdir -p "$OUTPUT_DIR"
        HASH=$(sha256sum ./data/style-agent.db | cut -d' ' -f1)
        tar -czf "$OUTPUT_DIR/$HASH.tar.gz" -C ./data style-agent.db
        echo "Index hash: $HASH"

        # Package fastembed model cache for init container
        if [ -d .fastembed_cache ]; then
          mkdir -p "$EMBEDDER_DIR"
          EMBED_HASH=$(find .fastembed_cache -type f -exec sha256sum {} + | sort | sha256sum | cut -d' ' -f1)
          tar -czf "$EMBEDDER_DIR/$EMBED_HASH.tar.gz" .fastembed_cache
          echo "Embedder hash: $EMBED_HASH"
        fi
      '';

      packages.deploy-galoy-agents = pkgs.writeShellScriptBin "deploy-galoy-agents" ''
        set -euo pipefail

        REPO_DIR="''${1:?Usage: deploy-galoy-agents <repo-dir>}"

        export PATH="${pkgs.lib.makeBinPath [
          pkgs.google-cloud-sdk
          pkgs.kubernetes-helm
          pkgs.kubectl
          pkgs.coreutils
          pkgs.yq-go
        ]}:$PATH"

        : "''${GCP_CREDS_JSON:?GCP_CREDS_JSON must be set}"
        : "''${GKE_CLUSTER:?GKE_CLUSTER must be set}"
        : "''${GKE_ZONE:?GKE_ZONE must be set}"
        : "''${GKE_PROJECT:?GKE_PROJECT must be set}"
        : "''${DEPLOY_NAMESPACE:?DEPLOY_NAMESPACE must be set}"
        : "''${IMAGE_DIGEST:?IMAGE_DIGEST must be set}"

        echo "=== Authenticating to GKE ==="
        echo "$GCP_CREDS_JSON" > /tmp/gcp-key.json
        trap 'rm -f /tmp/gcp-key.json' EXIT
        gcloud auth activate-service-account --key-file=/tmp/gcp-key.json
        gcloud container clusters get-credentials "$GKE_CLUSTER" \
          --region "$GKE_ZONE" \
          --project "$GKE_PROJECT"

        echo "=== Deploying galoy-agents ==="
        echo "Image digest: $IMAGE_DIGEST"
        echo "Namespace: $DEPLOY_NAMESPACE"

        cd "$REPO_DIR"

        helm dependency build charts/galoy-agents

        helm upgrade --install galoy-agents charts/galoy-agents \
          --namespace "$DEPLOY_NAMESPACE" \
          --create-namespace \
          --values charts/galoy-agents/values.yaml \
          --values ci/prod-values.yml \
          --set "galoyAgents.image.digest=$IMAGE_DIGEST" \
          --set "galoyAgents.image.tag=" \
          --timeout 600s \
          --wait

        echo "=== Waiting for rollout ==="
        kubectl rollout status deployment/galoy-agents \
          --namespace "$DEPLOY_NAMESPACE" \
          --timeout=300s

        echo "=== Deploy complete ==="
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
