{ pkgs, keycloak-mcp }:
# Minimal container image for the read-only Keycloak MCP upstream.
# The service speaks Streamable HTTP at /mcp and is intended to sit
# behind the deployment tunnel-connector.
pkgs.dockerTools.buildLayeredImage {
  name = "keycloak-mcp";
  tag = "latest";
  contents = [
    pkgs.cacert
    keycloak-mcp
  ];
  config = {
    Entrypoint = [ "${keycloak-mcp}/bin/keycloak-mcp" ];
    User = "65534:65534"; # nobody:nogroup
    Env = [
      "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
      "RUST_LOG=info"
    ];
  };
}
