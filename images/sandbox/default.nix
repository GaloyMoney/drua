{ pkgs, sandbox-tool-server }:
let
  passwd = pkgs.writeTextDir "etc/passwd" ''
    root:x:0:0:root:/root:/bin/bash
    agent:x:1000:1000:Agent:/workspace:/bin/bash
    nobody:x:65534:65534:Nobody:/:/sbin/nologin
  '';
  group = pkgs.writeTextDir "etc/group" ''
    root:x:0:
    agent:x:1000:
    nogroup:x:65534:
  '';

  # Git credential helper: reads token from /run/secrets/github-token.
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
      store|erase) ;;
    esac
  '';

  gitconfig = pkgs.writeTextDir "home/agent/.gitconfig" ''
    [credential "https://github.com"]
      helper = ${gitCredentialHelper}/bin/git-credential-github-token
    [user]
      name = galoy-agents[bot]
      email = galoy-agents[bot]@users.noreply.github.com
  '';
in
pkgs.dockerTools.buildLayeredImage {
  name = "sandbox";
  tag = "latest";
  contents = [
    passwd
    group
    pkgs.bashInteractive
    pkgs.coreutils
    pkgs.git
    pkgs.curl
    pkgs.cacert
    pkgs.findutils
    pkgs.gnused
    pkgs.gnugrep
    gitCredentialHelper
    gitconfig
    sandbox-tool-server
  ];
  fakeRootCommands = ''
    mkdir -p ./workspace
    chown 1000:1000 ./workspace
    mkdir -p ./tmp
    chmod 1777 ./tmp
  '';
  config = {
    Cmd = [ "${sandbox-tool-server}/bin/sandbox-tool-server" ];
    User = "1000:1000";
    WorkingDir = "/workspace";
    Env = [
      "HOME=/workspace"
      "USER=agent"
      "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
      "GIT_CONFIG_GLOBAL=/home/agent/.gitconfig"
    ];
    ExposedPorts = {
      "3000/tcp" = {};
    };
  };
}
