#!/usr/bin/env bash
# Index build throughput (target: at least 50 MB/s per core), in the dev container:
#   scripts/dev scripts/bench/index-build.sh [threads] [reps]
# Fetches the pinned corpora once into $CORPORA (default target/bench-corpora,
# the dev container's target volume): golang/go at go1.23.0 (shallow, bare), a
# 100 MB synthetic tree (scripts/bench/synthetic-tree.py), and with LARGE=1 the Linux
# kernel at v6.6. Then runs the release `build_bench` example (phase breakdown on
# one thread, full builds on 1 and N threads with and without symbols).
# CARGO_TARGET_DIR and CARGO_BUILD_JOBS are honoured; SKIP_BUILD=1 reuses the binary,
# SYMBOLS=0 leaves out symbol extraction.
set -euo pipefail
cd "$(dirname "$0")/../.."
THREADS=${1:-$(nproc)}
REPS=${2:-3}
CORPORA=${CORPORA:-target/bench-corpora}
mkdir -p "$CORPORA"

clone() { # name url tag
  [ -d "$CORPORA/$1" ] || git clone -q --bare --depth 1 --branch "$3" "$2" "$CORPORA/$1"
}
clone go.git https://github.com/golang/go go1.23.0
if [ ! -d "$CORPORA/synthetic.git" ]; then
  git init -q --bare "$CORPORA/synthetic.git.tmp"
  scripts/bench/synthetic-tree.py 100 | git -C "$CORPORA/synthetic.git.tmp" fast-import --quiet
  git -C "$CORPORA/synthetic.git.tmp" symbolic-ref HEAD refs/heads/main
  git -C "$CORPORA/synthetic.git.tmp" repack -adfq
  mv "$CORPORA/synthetic.git.tmp" "$CORPORA/synthetic.git"
fi
REPOS=("$CORPORA/go.git" "$CORPORA/synthetic.git")
if [ "${LARGE:-0}" = 1 ]; then
  clone linux.git https://github.com/torvalds/linux v6.6
  REPOS+=("$CORPORA/linux.git")
fi

TARGET=${CARGO_TARGET_DIR:-target}
if [ "${SKIP_BUILD:-0}" != 1 ]; then
  cargo build --locked --release -p sourcegull-indexer --example build_bench
fi
echo "host: $(nproc) CPUs, $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | sed 's/^ //')"
"$TARGET/release/examples/build_bench" "$CORPORA/out" "$THREADS" "$REPS" "${REPOS[@]}"
