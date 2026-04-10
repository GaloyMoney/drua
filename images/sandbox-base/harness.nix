{ pkgs }:
pkgs.buildNpmPackage {
  pname = "agent-harness";
  version = "0.1.0";
  src = ./agent-harness;
  npmDepsHash = "sha256-am0pOrbtqzZwpGmTRJDCcLXVTHo5m7uFCJYDDbq7ZfE=";

  nativeBuildInputs = [ pkgs.esbuild ];

  # Skip the default `npm run build`; we bundle with esbuild instead.
  dontNpmBuild = true;

  buildPhase = ''
    runHook preBuild
    esbuild src/index.ts \
      --bundle \
      --platform=node \
      --target=node22 \
      --format=esm \
      --banner:js="import { createRequire } from 'module'; const require = createRequire(import.meta.url);" \
      --outfile=dist/index.js
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    mkdir -p $out/lib
    cp dist/index.js $out/lib/index.js

    # cli.js is the Claude Code CLI binary — spawned as a persistent
    # subprocess by the harness.  It cannot be bundled by esbuild.
    mkdir -p $out/lib/sdk
    cp node_modules/@anthropic-ai/claude-agent-sdk/cli.js $out/lib/sdk/
    cp node_modules/@anthropic-ai/claude-agent-sdk/*.wasm  $out/lib/sdk/ 2>/dev/null || true
    cp -r node_modules/@anthropic-ai/claude-agent-sdk/vendor $out/lib/sdk/ 2>/dev/null || true
    runHook postInstall
  '';
}
