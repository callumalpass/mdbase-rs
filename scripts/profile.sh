#!/usr/bin/env bash
set -euo pipefail

cargo run --locked --release \
  --manifest-path ../mdbase-connect/Cargo.toml \
  -p mdbase-cli -- profile engine "$@"
