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
            craneLib.filterCargoSources path type;
        };

        commonArgs = {
          inherit src;
          strictDeps = true;
          SQLX_OFFLINE = true;
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

        packages.default = galoy-agents;

        packages.docker-image = pkgs.dockerTools.buildLayeredImage {
          name = "galoy-agents";
          tag = "latest";
          contents = [
            galoy-agents
            pkgs.cacert
          ];
          config = {
            Cmd = [ "${galoy-agents}/bin/galoy-agents" ];
            Env = [
              "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
            ];
          };
        };

        devShells.default = pkgs.mkShell {
          buildInputs = [
            rustToolchain
            pkgs.cargo-nextest
            pkgs.pkg-config
            pkgs.docker-compose
            pkgs.opentofu
            pkgs.ytt
            pkgs.kubernetes-helm
            pkgs.minikube
            pkgs.kubectl
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
