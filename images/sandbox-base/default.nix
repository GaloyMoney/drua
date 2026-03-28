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
  ];
  fakeRootCommands = ''
    mkdir -p ./home/agent
    chown 1000:1000 ./home/agent
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
