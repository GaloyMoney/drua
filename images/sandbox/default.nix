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

  # gVisor's PTY emulation breaks Nix builder setup messages for local
  # uncached builds. Use a pipe for builder stderr in the sandbox image.
  nixGvisorPtyPipePatch = pkgs.writeText "nix-gvisor-pty-pipe.patch" ''
    diff -ruN a/src/libstore/include/nix/store/build/derivation-builder.hh b/src/libstore/include/nix/store/build/derivation-builder.hh
    --- a/src/libstore/include/nix/store/build/derivation-builder.hh
    +++ b/src/libstore/include/nix/store/build/derivation-builder.hh
    @@ -141,12 +141,16 @@
         virtual ~DerivationBuilder() = default;
    ${" "}
         /**
    -     * Master side of the pseudoterminal used for the builder's
    -     * standard output/error.
    +     * Read side of the pipe used for the builder's standard error.
          */
         AutoCloseFD builderOut;
    ${" "}
    +    /**
    +     * Write side of the pipe used for the builder's standard error.
    +     */
    +    AutoCloseFD builderOutWrite;
    +
         /**
          * Set up build environment / sandbox, acquiring resources (e.g.
          * locks as needed). After this is run, the builder should be
          * started.
    diff -ruN a/src/libstore/unix/build/derivation-builder.cc b/src/libstore/unix/build/derivation-builder.cc
    --- a/src/libstore/unix/build/derivation-builder.cc
    +++ b/src/libstore/unix/build/derivation-builder.cc
    @@ -849,28 +849,14 @@
         /* Create the log file. */
         miscMethods->openLogFile();
    ${" "}
    -    /* Create a pseudoterminal to get the output of the builder. */
    -    builderOut = posix_openpt(O_RDWR | O_NOCTTY);
    -    if (!builderOut)
    -        throw SysError("opening pseudoterminal master");
    -
    -    std::string slaveName = getPtsName(builderOut.get());
    -
    -    if (buildUser) {
    -        chmod(slaveName, 0600);
    -
    -        if (chown(slaveName.c_str(), buildUser->getUID(), 0))
    -            throw SysError("changing owner of pseudoterminal slave");
    -    }
    -#ifdef __APPLE__
    -    else {
    -        if (grantpt(builderOut.get()))
    -            throw SysError("granting access to pseudoterminal slave");
    -    }
    -#endif
    -
    -    if (unlockpt(builderOut.get()))
    -        throw SysError("unlocking pseudoterminal");
    +    /* Create a pipe to get the output of the builder. gVisor's PTY
    +       emulation can drop the setup marker Nix writes before execing the
    +       builder, which makes every uncached build fail during environment
    +       initialisation. A pipe is enough for sandboxed agent builds. */
    +    Pipe builderOutPipe;
    +    builderOutPipe.create();
    +    builderOut = std::move(builderOutPipe.readSide);
    +    builderOutWrite = std::move(builderOutPipe.writeSide);
    ${" "}
         buildResult.startTime = time(nullptr);
    ${" "}
    @@ -878,6 +864,7 @@
         startChild();
    ${" "}
         pid.setSeparatePG(true);
    +    builderOutWrite.close();
    ${" "}
         processSandboxSetupMessages();
    ${" "}
    @@ -967,24 +954,14 @@
    ${" "}
     void DerivationBuilderImpl::openSlave()
     {
    -    std::string slaveName = getPtsName(builderOut.get());
    -
    -    AutoCloseFD builderOut = open(slaveName.c_str(), O_RDWR | O_NOCTTY);
    -    if (!builderOut)
    -        throw SysError("opening pseudoterminal slave");
    -
    -    // Put the pt into raw mode to prevent \n -> \r\n translation.
    -    struct termios term;
    -    if (tcgetattr(builderOut.get(), &term))
    -        throw SysError("getting pseudoterminal attributes");
    -
    -    cfmakeraw(&term);
    -
    -    if (tcsetattr(builderOut.get(), TCSANOW, &term))
    -        throw SysError("putting pseudoterminal into raw mode");
    +    if (!builderOutWrite)
    +        throw Error("builder output pipe write side is not open");
    ${" "}
    -    if (dup2(builderOut.get(), STDERR_FILENO) == -1)
    +    if (dup2(builderOutWrite.get(), STDERR_FILENO) == -1)
             throw SysError("cannot pipe standard error into log file");
    +
    +    builderOut.close();
    +    builderOutWrite.close();
     }
    ${" "}
     #if NIX_WITH_AWS_AUTH
     std::optional<AwsCredentials> DerivationBuilderImpl::preResolveAwsCredentials()
    diff -ruN a/src/libstore/unix/build/linux-derivation-builder.cc b/src/libstore/unix/build/linux-derivation-builder.cc
    --- a/src/libstore/unix/build/linux-derivation-builder.cc
    +++ b/src/libstore/unix/build/linux-derivation-builder.cc
    @@ -474,6 +474,7 @@
             sendPid.writeSide.close();
    ${"  "}
             if (helper.wait() != 0) {
    +            builderOutWrite.close();
                 processSandboxSetupMessages();
                 // Only reached if the child process didn't send an exception.
                 throw Error("unable to start build process");
  '';
  nix = pkgs.nix.appendPatches [
    nixGvisorPtyPipePatch
  ];

  # Credential helper: reads the projected K8s ServiceAccount token at
  # invocation time and hands it to git as a Bearer credential (git
  # 2.46+ authtype/credential protocol — see `gitcredentials(7)`).
  # Token rotates on disk; helper re-reads each call. Used for traffic
  # to the drua git-proxy (`$DRUA_GIT_PROXY_URL/git/...`), whose auth
  # middleware expects `Authorization: Bearer <SA JWT>`. The proxy
  # validates the JWT via TokenReview and mints a fresh GitHub App
  # installation token upstream — the sandbox never sees a GitHub
  # credential.
  gitCredentialHelper = pkgs.writeShellScriptBin "git-credential-drua-sa" ''
    case "$1" in
      get)
        token_path="''${DRUA_SA_TOKEN_PATH:-/var/run/secrets/galoy-agents-git/token}"
        if [ -f "$token_path" ]; then
          printf 'capability[]=authtype\n'
          printf 'authtype=Bearer\n'
          printf 'credential=%s\n' "$(cat "$token_path")"
          printf '\n'
        fi
        ;;
      store|erase) ;;
    esac
  '';

  # The static gitconfig — `insteadOf` rewriting is appended at runtime
  # by the entrypoint based on `$DRUA_GIT_PROXY_URL`, since gitconfig
  # files don't expand env vars.
  gitconfig = pkgs.writeTextDir "home/agent/.gitconfig.base" ''
    [user]
      name = drua[bot]
      email = drua[bot]@users.noreply.github.com
  '';

  # Entrypoint: starts nix-daemon, waits for socket, materialises the
  # dynamic gitconfig (rewriting github.com → DRUA_GIT_PROXY_URL when
  # set), then execs the tool server.
  entrypoint = pkgs.writeShellScriptBin "sandbox-entrypoint" ''
    set -euo pipefail

    mkdir -p /workspace/tmp /nix/var/nix/daemon-socket /var/empty
    chown 1000:1000 /workspace /workspace/tmp 2>/dev/null || true

    # Compose the agent's gitconfig: static base + dynamic insteadOf
    # block that points GitHub URLs at the drua git-proxy when
    # DRUA_GIT_PROXY_URL is set. The rewritten URL embeds a
    # `x-access-token@` username so git invokes the credential helper
    # proactively (no unauthenticated probe / 401 round-trip needed);
    # the helper then returns `authtype=Bearer credential=<SA JWT>` and
    # git sends `Authorization: Bearer <SA JWT>` to the proxy.
    # Helper-name must match the wrapper written into the image (no
    # path — git resolves via PATH).
    cp /home/agent/.gitconfig.base /home/agent/.gitconfig
    chown 1000:1000 /home/agent/.gitconfig 2>/dev/null || true
    if [ -n "''${DRUA_GIT_PROXY_URL:-}" ]; then
      proxy_no_scheme="''${DRUA_GIT_PROXY_URL#*://}"
      proxy_scheme="''${DRUA_GIT_PROXY_URL%%://*}"
      proxy_with_user="''${proxy_scheme}://x-access-token@''${proxy_no_scheme%/}/"
      cat >> /home/agent/.gitconfig <<EOF
[url "''${proxy_with_user}"]
    insteadOf = https://github.com/
    insteadOf = git@github.com:
[credential]
    helper = drua-sa
[http]
    extraHeader = X-Drua-Sandbox: 1
EOF
    fi

    # Clients use NIX_REMOTE=daemon, but the daemon itself must open the local
    # store or it recursively connects back to its own socket.
    env -u NIX_REMOTE ${nix}/bin/nix-daemon &

    for i in $(seq 1 100); do
      [ -S /nix/var/nix/daemon-socket/socket ] && break
      sleep 0.1
    done

    exec sandbox-tool-server
  '';

  # Init-container entrypoint for the per-sandbox /nix PVC. When the PVC
  # is empty, copy the image's baked-in /nix into it; otherwise no-op.
  # Must run inside *this* image (not busybox) — only this image carries
  # the seed contents. SEED_TARGET defaults to /seed.
  nixStoreSeed = pkgs.writeShellScriptBin "nix-store-seed" ''
    set -euo pipefail
    target="''${SEED_TARGET:-/seed}"
    if [ ! -d "$target" ]; then
      echo "nix-store-seed: target $target does not exist" >&2
      exit 1
    fi
    # `store` is the canonical sentinel — every populated /nix has it.
    if [ -d "$target/store" ] && [ -n "$(ls -A "$target/store" 2>/dev/null)" ]; then
      echo "nix-store-seed: $target already populated, skipping seed"
      exit 0
    fi
    echo "nix-store-seed: seeding $target from /nix..."
    cp -a /nix/. "$target/"
    echo "nix-store-seed: done"
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
    pkgs.cargo
    pkgs.cargo-nextest
    pkgs.gnumake
    nix
    gitCredentialHelper
    gitconfig
    entrypoint
    nixStoreSeed
    sandbox-tool-server
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
