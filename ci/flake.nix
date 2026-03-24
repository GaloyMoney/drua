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

            REPOS_DIR="''${1:?Usage: build-style-index <repos-dir> <output-dir>}"
            OUTPUT_DIR="''${2:?Usage: build-style-index <repos-dir> <output-dir>}"

            export STYLE_AGENT_CONFIG="$(pwd)/ci/config.style-agent.toml"
            export PATH="${pkgs.lib.makeBinPath [ style-agent ]}:$PATH"

            cd style-agent
            style-agent build-index --repos-dir "$REPOS_DIR"

            mkdir -p "$OUTPUT_DIR"
            HASH=$(sha256sum ./data/style-agent.db | cut -d' ' -f1)
            tar -czf "$OUTPUT_DIR/$HASH.tar.gz" -C ./data style-agent.db
            echo "Index hash: $HASH"
          '';
        in "${wrapped}/bin/build-style-index";
      };
    });
}
