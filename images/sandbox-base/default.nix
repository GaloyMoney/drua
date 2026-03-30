{ pkgs }:
let
  passwd = pkgs.writeTextDir "etc/passwd" ''
    root:x:0:0:root:/root:/bin/bash
    agent:x:1000:1000:Agent:/home/agent:/bin/bash
    nobody:x:65534:65534:Nobody:/:/sbin/nologin
  '';
  group = pkgs.writeTextDir "etc/group" ''
    root:x:0:
    agent:x:1000:
    nogroup:x:65534:
  '';
  shadow = pkgs.writeTextDir "etc/shadow" ''
    root:!:1::::::
    agent:!:1::::::
    nobody:!:1::::::
  '';
  nsswitch = pkgs.writeTextDir "etc/nsswitch.conf" ''
    hosts: files dns
  '';
  nixConf = pkgs.writeTextDir "etc/nix/nix.conf" ''
    experimental-features = nix-command flakes
    sandbox = false
    filter-syscalls = false
    extra-substituters = https://galoy-agents.cachix.org
    extra-trusted-public-keys = galoy-agents.cachix.org-1:wGb5wYMLJ3yPYoOvf2O5vt9gEZEJ7hHsqDCNPMELGPY=
    connect-timeout = 5
    fallback = true
  '';

  # Wrapper script for running the agent harness.
  # On first invocation it installs npm dependencies and compiles TypeScript.
  agentHarnessWrapper = pkgs.writeShellScriptBin "agent-harness" ''
    set -euo pipefail
    HARNESS_DIR=/opt/agent-harness
    if [ ! -d "$HARNESS_DIR/node_modules" ]; then
      (cd "$HARNESS_DIR" && ${pkgs.nodejs_22}/bin/npm install --no-fund --no-audit 2>&1 >&2)
    fi
    if [ ! -f "$HARNESS_DIR/dist/index.js" ]; then
      (cd "$HARNESS_DIR" && ${pkgs.nodejs_22}/bin/npx tsc 2>&1 >&2)
    fi
    exec ${pkgs.nodejs_22}/bin/node "$HARNESS_DIR/dist/index.js" "$@"
  '';
in
pkgs.dockerTools.buildLayeredImage {
  name = "sandbox-base";
  tag = "latest";
  contents = [
    passwd
    group
    shadow
    nsswitch
    nixConf
    pkgs.bashInteractive
    pkgs.coreutils
    pkgs.git
    pkgs.curl
    pkgs.cacert
    pkgs.nix
    pkgs.gnutar
    pkgs.gzip
    pkgs.xz
    pkgs.findutils
    pkgs.gnused
    pkgs.gnugrep
    pkgs.openssh
    pkgs.gh
    pkgs.nodejs_22
    agentHarnessWrapper
  ];
  fakeRootCommands = ''
    mkdir -p ./home/agent
    chown 1000:1000 ./home/agent
    mkdir -p ./tmp
    chmod 1777 ./tmp

    # Install agent harness source to /opt.
    # npm install + tsc happen on first invocation inside the sandbox.
    mkdir -p ./opt/agent-harness/src
    cp ${./agent-harness/package.json} ./opt/agent-harness/package.json
    cp ${./agent-harness/tsconfig.json} ./opt/agent-harness/tsconfig.json
    cp ${./agent-harness/src/index.ts} ./opt/agent-harness/src/index.ts
    chmod -R 777 ./opt/agent-harness
  '';
  config = {
    Cmd = [ "/bin/bash" ];
    User = "1000:1000";
    WorkingDir = "/home/agent";
    Env = [
      "HOME=/home/agent"
      "USER=agent"
      "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
      "NIX_SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
    ];
  };
}
