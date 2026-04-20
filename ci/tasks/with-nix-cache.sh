#!/bin/sh
set -eu

# Wrapper that adds a persistent local Nix binary cache.
# Concourse task caches persist /nix-cache between runs of the same task step.
# On the first run the cache is empty; on subsequent runs Nix finds most store
# paths locally and skips downloading them from cache.nixos.org.

# Maximum cache size in bytes (25 GB). When exceeded after saving,
# the oldest .narinfo / .nar files are removed until size drops below.
CACHE_MAX_BYTES=$((25 * 1024 * 1024 * 1024))

setup_nix_cache() {
  mkdir -p /nix-cache
  if [ -f /nix-cache/nix-cache-info ]; then
    cache_size=$(du -sh /nix-cache 2>/dev/null | cut -f1)
    echo "nix-cache: local cache found (${cache_size}), restoring"
    # Modify nix.conf for tools that read it directly
    echo "extra-substituters = file:///nix-cache" >> /etc/nix/nix.conf
    echo "require-sigs = false" >> /etc/nix/nix.conf

    # Export NIX_CONFIG so the running nix-daemon picks up changes.
    # The daemon loads nix.conf at startup (before this wrapper runs),
    # so modifying the file alone is not enough. NIX_CONFIG is read by
    # nix commands at invocation time and supplements the daemon config.
    export NIX_CONFIG="${NIX_CONFIG:+$NIX_CONFIG
}extra-substituters = file:///nix-cache
require-sigs = false"
  else
    echo "nix-cache: no local cache found (cold start)"
  fi
}

# Remove oldest files until cache is under CACHE_MAX_BYTES.
prune_nix_cache() {
  echo "nix-cache: calculating cache size..."
  current_bytes=$(du -sb /nix-cache 2>/dev/null | cut -f1)
  echo "nix-cache: cache size is $((current_bytes / 1024 / 1024)) MB"

  if [ "$current_bytes" -le "$CACHE_MAX_BYTES" ]; then
    echo "nix-cache: under limit ($((CACHE_MAX_BYTES / 1024 / 1024 / 1024)) GB), no pruning needed"
    return
  fi

  echo "nix-cache: over limit, pruning to ~25 GB..."

  # Delete oldest files first (both .narinfo and .nar) until under budget.
  # Re-check size every 50 deletions to avoid running du on every iteration.
  count=0
  ls -tr /nix-cache/*.narinfo /nix-cache/nar/* > /tmp/files-to-prune.txt 2>/dev/null || true

  while read -r file; do
    echo "nix-cache: removing $file"
    rm -f "$file"

    count=$((count + 1))
    if [ $((count % 50)) -eq 0 ]; then
      echo "nix-cache: deleted $count files so far, recalculating size..."
      current_bytes=$(du -sb /nix-cache 2>/dev/null | cut -f1)
      echo "nix-cache: cache size is $((current_bytes / 1024 / 1024)) MB"
      if [ "$current_bytes" -le "$CACHE_MAX_BYTES" ]; then
        break
      fi
    fi
  done < /tmp/files-to-prune.txt
  rm -f /tmp/files-to-prune.txt

  echo "nix-cache: pruning done, removed $count files, final size: $((current_bytes / 1024 / 1024)) MB"
}

save_nix_cache() {
  # Always save -- nix copy skips paths already present in the destination,
  # so this is cheap on warm runs but ensures newly-built derivations
  # (updated deps, new flake inputs) are cached for next time.
  echo "nix-cache: saving new store paths to local cache..."
  nix copy --to 'file:///nix-cache?compression=none' --all 2>/dev/null || true
  prune_nix_cache
  echo "nix-cache: done saving to local cache"
}

echo "=== with-nix-cache: start $(date -u +%H:%M:%S) ==="
setup_nix_cache
trap save_nix_cache EXIT
echo "=== with-nix-cache: setup done $(date -u +%H:%M:%S), running task ==="

"$@"
