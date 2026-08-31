#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
connect_root="${MDBASE_CONNECT_ROOT:-$repo_root/../mdbase-connect}"
feature='mdbase feature "legacy-collection-mutation"'

check_graph() {
  local label="$1"
  shift
  local tree
  tree="$(cargo tree -e features "$@")"
  if grep -Fq "$feature" <<<"$tree"; then
    echo "$label resolved the forbidden legacy-collection-mutation feature" >&2
    grep -F "$feature" <<<"$tree" >&2
    return 1
  fi
  echo "$label feature graph is legacy-free"
}

for package in mdbase-command mdbase-runtime mdbase-testbed-adapter; do
  check_graph \
    "$package" \
    --manifest-path "$repo_root/Cargo.toml" \
    -p "$package"
done

if [[ -f "$connect_root/Cargo.toml" ]]; then
  check_graph \
    "mdbase Connect workspace" \
    --manifest-path "$connect_root/Cargo.toml"
else
  echo "Connect workspace not found at $connect_root; set MDBASE_CONNECT_ROOT to check it" >&2
fi
