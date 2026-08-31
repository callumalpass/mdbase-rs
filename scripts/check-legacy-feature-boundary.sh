#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$repo_root/tests/compile/legacy-feature-boundary/Cargo.toml"
target_dir="$repo_root/target/legacy-feature-boundary"
errors="$target_dir/no-default-errors.txt"
lock_file="$(dirname "$fixture")/Cargo.lock"
trap 'rm -f "$errors" "$lock_file"' EXIT

cargo check --manifest-path "$fixture" --target-dir "$target_dir" --quiet

if cargo check --manifest-path "$fixture" --target-dir "$target_dir" --no-default-features 2>"$errors"; then
  echo "legacy facade unexpectedly compiled without legacy-collection-mutation" >&2
  exit 1
fi

for method in create update delete rename backfill batch_update batch_delete; do
  if ! grep -Fq "no method named \`$method\`" "$errors"; then
    echo "no-default fixture did not prove Collection::$method absent" >&2
    cat "$errors" >&2
    exit 1
  fi
done

echo "legacy feature boundary passed: default facade present, no-default facade absent"
