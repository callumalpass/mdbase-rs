#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
spec_root="${MDBASE_SPEC_ROOT:-"$repository_root/../mdbase-spec"}"
source_dir="$spec_root/standard-packs/mdbase-runtime/0.2.0/schemas"
target_dir="$repository_root/crates/mdbase-runtime/schemas"

schema_ids=(
  workflow
  policy
  provider-registration
  capability-grant
  run
  action-attempt
  checkpoint
  timer
  diagnostic
  dead-letter
)

if [[ ! -d "$source_dir" ]]; then
  echo "Runtime 0.2 schemas not found at $source_dir" >&2
  echo "Set MDBASE_SPEC_ROOT to a checkout of mdbase-spec." >&2
  exit 1
fi

mkdir -p "$target_dir"
for schema_id in "${schema_ids[@]}"; do
  cp \
    "$source_dir/mdbase.runtime.$schema_id/1.0.0.schema.json" \
    "$target_dir/runtime-$schema_id.schema.json"
done

echo "Synchronized ${#schema_ids[@]} Runtime 0.2 schemas from $source_dir"
