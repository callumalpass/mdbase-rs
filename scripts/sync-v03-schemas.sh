#!/usr/bin/env sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
spec_root=${MDBASE_SPEC_ROOT:-"$repo_root/../mdbase-spec"}
source_dir="$spec_root/schemas/v0.3"
target_dir="$repo_root/schemas/v0.3"

mkdir -p "$target_dir"
for schema in config data-contract diagnostic operation-result query query-result type-file type-pack view; do
	cp "$source_dir/$schema.schema.json" "$target_dir/$schema.schema.json"
done

mkdir -p "$target_dir/runtime"
for schema in action capability checkpoint diagnostic event-envelope event provider run runtime-policy timer workflow; do
	cp "$source_dir/runtime/$schema.schema.json" "$target_dir/runtime/$schema.schema.json"
done

echo "Synced mdbase v0.3 schemas from $source_dir"
