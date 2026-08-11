#!/usr/bin/env bash
set -euo pipefail

# Non-gating Phase 0 observations. The raw JSON and Markdown report are written
# to target/ by default; pass MDBASE_PHASE0_OUTPUT to retain them elsewhere.
repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
connect_dir="${MDBASE_CONNECT_DIR:-/home/calluma/projects/worktrees/stabilization/mdbase-connect}"
output_dir="${MDBASE_PHASE0_OUTPUT:-${repo_dir}/target/benchmarks/phase0-baseline}"
records="${MDBASE_PHASE0_RECORDS:-2000,10000}"
mixed_threads="${MDBASE_PHASE0_MIXED_THREADS:-4}"
mixed_rounds="${MDBASE_PHASE0_MIXED_ROUNDS:-40}"
rss_requests="${MDBASE_PHASE0_RSS_REQUESTS:-1600}"

mkdir -p "${output_dir}"
export MDBASE_RS_COMMIT="$(git -C "${repo_dir}" rev-parse HEAD)"
export MDBASE_CONNECT_COMMIT="$(git -C "${connect_dir}" rev-parse HEAD)"

cd "${repo_dir}"
cargo run --locked --release --bin phase0-baseline -- \
  --records "${records}" \
  --output-dir "${output_dir}" \
  --mixed-threads "${mixed_threads}" \
  --mixed-rounds "${mixed_rounds}" \
  --rss-requests "${rss_requests}"
