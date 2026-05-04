{ pkgs }:
# Concourse resource for triggering drua workflow runs from a pipeline.
# Mirrors github.com/GaloyMoney/concourse-zenduty-resource in shape:
# - `out` POSTs to drua's `/webhooks/{definition_id}` (the receiver in
#   server/src/webhook.rs) with a Concourse build-context payload
# - `check` and `in` are stubs (no upstream version stream, no fetch)
#
# Image is published from this repo's CI as
# `<gar>/concourse-drua-resource:edge`. Pipelines reference it via a
# registry-image resource_type.
let
  runtimeInputs = with pkgs; [
    bashInteractive
    coreutils
    curl
    cacert
    jq
  ];

  mkResourceScript = name: source: pkgs.writeShellApplication {
    inherit name runtimeInputs;
    text = builtins.readFile source;
  };

  checkScript = mkResourceScript "check" ./check.sh;
  inScript = mkResourceScript "in" ./in.sh;
  outScript = mkResourceScript "out" ./out.sh;

  # Concourse invokes `/opt/resource/{check,in,out}` directly. The
  # `writeShellApplication` derivations carry Nix-store shebangs and
  # the runtime inputs on PATH, so we just stage them at the canonical
  # paths.
  resourceDir = pkgs.runCommand "concourse-drua-resource-tree" { } ''
    mkdir -p $out/opt/resource
    cp ${checkScript}/bin/check $out/opt/resource/check
    cp ${inScript}/bin/in       $out/opt/resource/in
    cp ${outScript}/bin/out     $out/opt/resource/out
    chmod 0755 $out/opt/resource/check $out/opt/resource/in $out/opt/resource/out
  '';
in
pkgs.dockerTools.buildLayeredImage {
  name = "concourse-drua-resource";
  tag = "latest";
  contents = [
    pkgs.cacert
    resourceDir
  ];
  config = {
    Env = [
      "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
    ];
  };
}
