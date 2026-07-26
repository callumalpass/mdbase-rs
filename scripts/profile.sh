#!/usr/bin/env bash
set -euo pipefail

cargo run --locked --release --bin mdb-profile -- "$@"
