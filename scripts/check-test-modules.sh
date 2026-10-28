#!/usr/bin/env bash
# Every integration test file of a crate with `autotests = false` is declared in its
# tests/main.rs or as a [[test]] target; an undeclared file would never be compiled or run.
set -euo pipefail
cd "$(dirname "$0")/.."
status=0
for manifest in crates/*/Cargo.toml; do
  grep -q '^autotests = false' "$manifest" || continue
  dir=$(dirname "$manifest")
  for f in "$dir"/tests/*.rs; do
    name=$(basename "$f" .rs)
    [ "$name" = main ] && continue
    grep -q "^mod $name;" "$dir/tests/main.rs" && continue
    grep -q "^name = \"$name\"" "$manifest" && continue
    echo "$f is not declared in $dir/tests/main.rs" >&2
    status=1
  done
done
exit $status
