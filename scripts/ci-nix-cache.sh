#!/usr/bin/env bash
# Keep compiled check outputs and dependencies in one GitHub cache archive.
# Usage: ci-nix-cache.sh save <absolute-cache-dir> <installable>...
#
# Exclude publicly cached paths, vendored sources, and toolchain downloads.
# Keep compiled Cargo dependency outputs: these avoid rebuilding the workspace
# dependency graph when application code changes. Nix validates store paths
# when it uses a restored file binary cache. Replace the previous generation
# instead of accumulating obsolete outputs in every saved archive.
set -euo pipefail

usage() { echo "usage: $0 save <cache-dir> <installable>..." >&2; exit 2; }
[[ $# -ge 3 && $1 == save ]] || usage
cache_dir=$2
shift 2
[[ $cache_dir == /* ]] || { echo "cache directory must be absolute" >&2; exit 2; }

mapfile -t derivations < <(nix path-info --derivation "$@")
mapfile -t candidates < <(
  nix-store --query --requisites --include-outputs "${derivations[@]}" \
    | grep -v '\.drv$' \
    | grep -Ev -- '-(source|vendor-registry|vendor-cargo-deps|nix-shell-env)$|/[a-z0-9]{32}-(cargo-src|rust)-' \
    | while read -r path; do [[ -e $path ]] && printf '%s\n' "$path"; done
)
[[ ${#candidates[@]} -gt 0 ]] || { echo "nothing to keep" >&2; exit 0; }

# Newer Nix prints an object keyed by store path, older Nix an array. Files
# added to the store (no deriver) are cheap to add again.
mapfile -t keep < <(
  nix path-info --json "${candidates[@]}" | jq -r '
    (if type == "array" then .[] else (to_entries[] | .value + {path: .key}) end)
    | select(.deriver != null)
    | select(any(.signatures[]?; startswith("cache.nixos.org-1:")) | not)
    | .path'
)
[[ ${#keep[@]} -gt 0 ]] || { echo "nothing to keep" >&2; exit 0; }

rm -rf -- "$cache_dir"
nix copy --to "file://$cache_dir?compression=zstd" "${keep[@]}"
echo "Kept ${#keep[@]} outputs, $(du -sh "$cache_dir" | cut -f1) with their closures:"
nix path-info --closure-size --human-readable "${keep[@]}" | sort -k2 -h
