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
    in {
      formatter = pkgs.alejandra;

      apps.build-style-index = {
        type = "app";
        program = let
          wrapped = pkgs.writeShellScriptBin "build-style-index" ''
            set -euo pipefail

            REPOS_DIR="''${1:?Usage: build-style-index <repos-dir> <gcs-bucket>}"
            GCS_BUCKET="''${2:?Usage: build-style-index <repos-dir> <gcs-bucket>}"

            export STYLE_AGENT_CONFIG="$(pwd)/ci/config.style-agent.toml"
            export PATH="${pkgs.lib.makeBinPath [
              style-agent
              pkgs.google-cloud-sdk
            ]}:$PATH"

            cd style-agent
            style-agent build-index --repos-dir "$REPOS_DIR"

            tar -czf /tmp/style-agent-index.tar.gz -C ./data style-agent.db
            gsutil cp /tmp/style-agent-index.tar.gz \
              "gs://$GCS_BUCKET/index/style-agent-index.tar.gz"
            echo "Uploaded to gs://$GCS_BUCKET/index/style-agent-index.tar.gz"
          '';
        in "${wrapped}/bin/build-style-index";
      };
    });
}
