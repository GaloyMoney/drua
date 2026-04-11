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

  # Agent harness — pre-built at image build time (zero runtime downloads).
  # Bypasses the Claude Agent SDK: the harness spawns cli.js directly as a
  # persistent subprocess (stdin/stdout stream-json) instead of using the
  # SDK's query() which spawns a new process per message (~25s in gVisor).
  agentHarness = import ./harness.nix { inherit pkgs; };

  # Thin wrapper — just exec node with the pre-built bundle.
  agentHarnessWrapper = pkgs.writeShellScriptBin "agent-harness" ''
    exec ${pkgs.nodejs_22}/bin/node ${agentHarness}/lib/index.js "$@"
  '';

  # ── GitHub App token wrappers ──────────────────────────────────────

  # Wrapped `gh` CLI: reads GH_TOKEN from /run/secrets/github-token on
  # every invocation so mid-session token rotation is transparent.
  ghWrapped = pkgs.writeShellScriptBin "gh" ''
    if [ -f /run/secrets/github-token ]; then
      export GH_TOKEN
      GH_TOKEN="$(cat /run/secrets/github-token)"
    fi
    exec ${pkgs.gh}/bin/gh "$@"
  '';

  # Git credential helper: reads the token from /run/secrets/github-token
  # and outputs protocol/host/username/password for github.com HTTPS.
  gitCredentialHelper = pkgs.writeShellScriptBin "git-credential-github-token" ''
    case "$1" in
      get)
        github=""
        while IFS= read -r line; do
          case "$line" in
            host=github.com) github=1 ;;
            "") break ;;
          esac
        done
        if [ "$github" = "1" ] && [ -f /run/secrets/github-token ]; then
          echo "protocol=https"
          echo "host=github.com"
          echo "username=x-access-token"
          echo "password=$(cat /run/secrets/github-token)"
          echo ""
        fi
        ;;
      store|erase)
        # No-op — we don't persist credentials
        ;;
    esac
  '';

  # Default .gitconfig with credential helper and bot identity.
  gitconfig = pkgs.writeTextDir "home/agent/.gitconfig" ''
    [credential "https://github.com"]
      helper = ${gitCredentialHelper}/bin/git-credential-github-token
    [user]
      name = galoy-agents[bot]
      email = galoy-agents[bot]@users.noreply.github.com
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
    ghWrapped
    gitCredentialHelper
    gitconfig
    pkgs.nodejs_22
    agentHarnessWrapper
  ];
  fakeRootCommands = ''
    mkdir -p ./home/agent
    chown 1000:1000 ./home/agent
    mkdir -p ./workspace
    chown 1000:1000 ./workspace
    mkdir -p ./tmp
    chmod 1777 ./tmp
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
