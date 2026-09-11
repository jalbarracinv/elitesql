#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

git rev-parse HEAD
rustc -Vv
node --version
python3 --version
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --locked
python3 -m unittest discover -s bindings/python/tests
node bindings/node/test.js
node bindings/node/test-integration.js

# Exercise external sort with a descriptor ceiling below the number of runs.
(
  ulimit -n 128
  cargo test --locked -p elitesql-core --test query_memory order_by_spills_under_a_tiny_budget_and_cleans_up
)
