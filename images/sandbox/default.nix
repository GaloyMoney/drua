{ pkgs, sandbox-tool-server }:
let
  passwd = pkgs.writeTextDir "etc/passwd" ''
    root:x:0:0:root:/root:/bin/bash
    agent:x:1000:1000:Agent:/workspace:/bin/bash
    nobody:x:65534:65534:Nobody:/:/sbin/nologin
    nixbld1:x:30001:30000:Nix build user 1:/var/empty:/sbin/nologin
    nixbld2:x:30002:30000:Nix build user 2:/var/empty:/sbin/nologin
    nixbld3:x:30003:30000:Nix build user 3:/var/empty:/sbin/nologin
    nixbld4:x:30004:30000:Nix build user 4:/var/empty:/sbin/nologin
    nixbld5:x:30005:30000:Nix build user 5:/var/empty:/sbin/nologin
    nixbld6:x:30006:30000:Nix build user 6:/var/empty:/sbin/nologin
    nixbld7:x:30007:30000:Nix build user 7:/var/empty:/sbin/nologin
    nixbld8:x:30008:30000:Nix build user 8:/var/empty:/sbin/nologin
  '';
  group = pkgs.writeTextDir "etc/group" ''
    root:x:0:
    agent:x:1000:
    nogroup:x:65534:
    nixbld:x:30000:nixbld1,nixbld2,nixbld3,nixbld4,nixbld5,nixbld6,nixbld7,nixbld8
  '';

  nixConf = pkgs.writeTextDir "etc/nix/nix.conf" ''
    build-users-group = nixbld
    experimental-features = nix-command flakes
    sandbox = false
  '';

  # Git credential helper: reads token from /run/secrets/github-token.
  # Used by the root server process as a fallback; the agent user uses
  # the workspace-level .git-credentials file written during /initialize.
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
      name = drua[bot]
      email = drua[bot]@users.noreply.github.com
  '';

  # Entrypoint: starts nix-daemon, waits for socket, then execs tool server.
  entrypoint = pkgs.writeShellScriptBin "sandbox-entrypoint" ''
    set -euo pipefail

    mkdir -p /workspace/tmp /nix/var/nix/daemon-socket /var/empty
    chown 1000:1000 /workspace /workspace/tmp 2>/dev/null || true

    nix-daemon &

    for i in $(seq 1 100); do
      [ -S /nix/var/nix/daemon-socket/socket ] && break
      sleep 0.1
    done

    exec sandbox-tool-server
  '';
in
pkgs.dockerTools.buildLayeredImage {
  name = "sandbox";
  tag = "latest";
  contents = [
    passwd
    group
    nixConf
    pkgs.bashInteractive
    pkgs.coreutils
    pkgs.gitMinimal
    pkgs.curl
    pkgs.cacert
    pkgs.findutils
    pkgs.gnused
    pkgs.gnugrep
    pkgs.ripgrep
    pkgs.nix
    gitCredentialHelper
    gitconfig
    entrypoint
    sandbox-tool-server
  ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
    pkgs.bubblewrap
  ];
  fakeRootCommands = ''
    mkdir -p ./workspace
    chown 1000:1000 ./workspace
    mkdir -p ./tmp
    chmod 1777 ./tmp
    mkdir -p ./var/empty
    mkdir -p ./nix/var/nix/daemon-socket
  '';
  config = {
    Cmd = [ "${entrypoint}/bin/sandbox-entrypoint" ];
    User = "0:0";
    WorkingDir = "/workspace";
    Env = [
      "HOME=/workspace"
      "USER=root"
      "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
      "GIT_CONFIG_GLOBAL=/home/agent/.gitconfig"
      "NIX_REMOTE=daemon"
    ];
    ExposedPorts = {
      "3000/tcp" = {};
    };
  };
}
