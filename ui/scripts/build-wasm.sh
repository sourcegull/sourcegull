#!/usr/bin/env sh
# Builds the query parser for the query box (crates/query-wasm, docs/PLAN.md §6.25) into
# static/query.wasm. Needs Rust with the wasm32-unknown-unknown target
# (`rustup target add wasm32-unknown-unknown`). Without the file the UI still works, with plain
# query input (no highlighting or diagnostics).
set -eu
cd "$(dirname "$0")/../.."
cargo build --release --locked -p sourcegull-query-wasm --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/sourcegull_query_wasm.wasm ui/static/query.wasm
echo "wrote ui/static/query.wasm ($(wc -c < ui/static/query.wasm) bytes)"
