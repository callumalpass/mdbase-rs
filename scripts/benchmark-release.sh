#!/usr/bin/env bash
set -euo pipefail

profile_report="${MDBASE_BENCH_REPORT:-target/benchmarks/release.json}"

cargo run --locked --release \
  --manifest-path ../mdbase-connect/Cargo.toml \
  -p mdbase-cli -- profile engine \
  --scenario all \
  --files 5000 \
  --projects 80 \
  --rename-refs 100 \
  --open-iters 10 \
  --read-iters 200 \
  --query-iters 10 \
  --view-iters 10 \
  --editor-iters 3 \
  --update-iters 20 \
  --rename-iters 5 \
  --create-iters 20 \
  --delete-iters 20 \
  --cache-rebuild-iters 1 \
  --seed 42 \
  --output "${profile_report}" \
  --thresholds benchmarks/release-v1.json
