{ pkgs, tunnel-connector }:
# Minimal container image for the outbound tunnel-connector. Runs in a
# target deployment cluster and dials *out* to drua over WebSocket.
# Shape is intentionally narrow — one Rust binary plus CA certs.
#
# No shell, no nix-daemon, no writable workspace, no user passwd/group
# files. Clap in `tunnel-connector` reads all config from
# `TUNNEL_*` env vars, which is how the galoy-charts tunnel-connector
# chart wires it.
pkgs.dockerTools.buildLayeredImage {
  name = "tunnel-connector";
  tag = "latest";
  contents = [
    pkgs.cacert
    tunnel-connector
  ];
  config = {
    Entrypoint = [ "${tunnel-connector}/bin/tunnel-connector" ];
    User = "65534:65534"; # nobody:nogroup
    Env = [
      "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
      "RUST_LOG=info"
    ];
  };
}
