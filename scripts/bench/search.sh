#!/usr/bin/env bash
# Run HTTP search measurements against one local Git repository or a benchmark corpus.
# Usage: scripts/bench/search.sh /path/to/git/repo [search-stream.py options]
#        scripts/bench/search.sh --corpus <name> [search-stream.py options]
# A corpus (tiny, small-repos, gitlab-dist, linux, linux-full, mixed-10g) is set up first with
# scripts/bench/corpus.py under $SOURCEGULL_CORPUS_DIR (idempotent; quick when complete) and
# verified, and its queries.json is used unless --queries is given. In the toolbox:
# `scripts/dev bench-search ...` (docs/development.md).
# Set PROFILE=release for an optimized binary, SKIP_BUILD=1 to reuse it,
# SOURCEGULL_PGDATA for a non-default local Postgres directory (otherwise $SOURCEGULL_DATABASE_URL's
# server is used when set, as in the toolbox), PORT to change the listener, WAIT_INDEX=0 for
# commit-only corpora, INDEX_TIMEOUT (seconds, default 7200) to bound the wait for indexing,
# COPY_COMMIT_LOG=1 to reuse a bare source repo's metadata log, OUT to retain raw JSON (for a
# corpus the default is $SOURCEGULL_CORPUS_DIR/results/<corpus>-<time>.json), and KEEP_WORK=1 to
# keep a corpus run's work directory. A private database is created per run.
# BENCH_HOOK replaces the stream benchmark with a command run once every repository is indexed;
# it sees BENCH_BASE, BENCH_WORK,
# BENCH_DATA, BENCH_SERVER_PID, BENCH_INDEX_SECS, BENCH_REPOS (a TSV of id, name and mirror
# path), BENCH_TRACKED_BYTES and SOURCEGULL_DATABASE_URL. Sourcegull reads the rest of its settings
# (SOURCEGULL_*) from the environment as usual.
set -euo pipefail
cd "$(dirname "$0")/../.."
export LC_ALL=C
CORPUS=
if [ "${1:-}" = --corpus ]; then
  CORPUS=${2:?usage: scripts/bench/search.sh --corpus <name> [benchmark options]}
  shift 2
  CDIR=$(python3 scripts/bench/corpus.py fetch "$CORPUS")
  python3 scripts/bench/corpus.py verify "$CORPUS"
  CORPUS_ROOT=$(dirname "$CDIR")
else
  SOURCE=${1:?usage: scripts/bench/search.sh /path/to/git/repo [benchmark options]}
  shift
  SOURCE=$(realpath "$SOURCE")
  git -C "$SOURCE" rev-parse --git-dir >/dev/null
fi
PORT=${PORT:-7090}
if [ -n "$CORPUS" ]; then
  # Next to the corpus, so the mirrors are hard links and the index lands on the same disk.
  mkdir -p "$CORPUS_ROOT/.work" "$CORPUS_ROOT/results"
  WORK=$(mktemp -d "$CORPUS_ROOT/.work/$CORPUS.XXXXXX")
  OUT=${OUT:-$CORPUS_ROOT/results/$CORPUS-$(date -u +%Y%m%dT%H%M%SZ).json}
else
  WORK=$(mktemp -d "${TMPDIR:-/tmp}/sourcegull-bench.XXXXXX")
  OUT=${OUT:-$WORK/results.json}
fi
DB="sourcegull_bench_$$"
if [ -z "${SOURCEGULL_PGDATA:-}" ] && [ -n "${SOURCEGULL_DATABASE_URL:-}" ]; then
  PG_SERVER=${SOURCEGULL_DATABASE_URL%/*}
elif [ -n "${SOURCEGULL_PGDATA:-}" ]; then
  PG_SERVER=$(scripts/dev-postgres.sh "$SOURCEGULL_PGDATA"); PG_SERVER=${PG_SERVER%/*}
else
  PG_SERVER=$(scripts/dev-postgres.sh); PG_SERVER=${PG_SERVER%/*}
fi
psql "$PG_SERVER/postgres" -qc "CREATE DATABASE $DB"
SERVER=
cleanup() {
  if [ -n "$SERVER" ]; then kill "$SERVER" 2>/dev/null || true; wait "$SERVER" 2>/dev/null || true; fi
  psql "$PG_SERVER/postgres" -qc "DROP DATABASE IF EXISTS $DB" || true
  if [ -n "$CORPUS" ] && [ "${KEEP_WORK:-0}" != 1 ]; then
    rm -rf "$WORK"
  else
    echo "benchmark work directory: $WORK"
  fi
}
trap cleanup EXIT
export SOURCEGULL_DATABASE_URL="$PG_SERVER/$DB"
psql_() { psql "$SOURCEGULL_DATABASE_URL" -tAqc "$1"; }
now() { date +%s.%N; }
elapsed() { awk -v a="$1" -v b="$(now)" 'BEGIN {printf "%.1f", b - a}'; }

PROFILE=${PROFILE:-debug}
TARGET=${CARGO_TARGET_DIR:-target}
case "$PROFILE" in
  debug) BIN=$TARGET/debug/sourcegull; BUILD_ARGS=() ;;
  release) BIN=$TARGET/release/sourcegull; BUILD_ARGS=(--release) ;;
  *) echo "PROFILE must be debug or release" >&2; exit 2 ;;
esac
if [ "${SKIP_BUILD:-}" != 1 ]; then cargo build -p sourcegull ${BUILD_ARGS[@]+"${BUILD_ARGS[@]}"}; fi
"$BIN" migrate up
NODE="http://127.0.0.1:$((PORT + 2))"

# The repositories to serve, "<repo name>\t<git dir>" per line.
if [ -n "$CORPUS" ]; then
  awk -F'\t' -v c="$CORPUS" -v d="$CDIR/repos" \
    '{print "gitlab.example.com/bench/" c "/" $1 "\t" d "/" $1 ".git"}' "$CDIR/manifest.tsv"
else
  printf 'gitlab.example.com/benchmark/corpus\t%s\n' "$SOURCE"
fi | sort -t$'\t' -k1,1 > "$WORK/sources.tsv"
REPOS=$(wc -l < "$WORK/sources.tsv")
# One statement registers every repository as cloned on this node; it prints "<id>\t<name>".
awk -F'\t' -v node="$NODE" -v q="'" '
  BEGIN { print "WITH src(name, ext) AS (VALUES" }
  { printf "%s(%s%s%s, %s%d%s)\n", (NR > 1 ? "," : ""), q, $1, q, q, NR, q }
  END {
    print "), ins AS (INSERT INTO repo (name, external_id, external_service_type,"
    print "    external_service_id, description)"
    print "  SELECT name, ext, " q "gitlab" q ", " q "https://gitlab.example.com/" q ", " q "Benchmark corpus" q " FROM src"
    print "  RETURNING id, name),"
    print "gs AS (INSERT INTO gitserver_repos (repo_id, clone_status, node)"
    print "  SELECT id, " q "cloned" q ", " q node q " FROM ins)"
    print "SELECT id || chr(9) || name FROM ins;"
  }' "$WORK/sources.tsv" > "$WORK/register.sql"
psql "$SOURCEGULL_DATABASE_URL" -tAq -v ON_ERROR_STOP=1 -f "$WORK/register.sql" |
  sort -t$'\t' -k2,2 > "$WORK/ids.tsv"
# Mirror each repository into the node's layout, data/repos/<id % 256 in hex>/<id>.git.
join -t$'\t' -1 2 -2 1 "$WORK/ids.tsv" "$WORK/sources.tsv" |
  awk -F'\t' -v data="$WORK/data/repos" '{printf "%s\t%s/%02x/%s.git\n", $3, data, $2 % 256, $2}' \
  > "$WORK/mirrors.tsv"
test "$(wc -l < "$WORK/mirrors.tsv")" = "$REPOS"
cut -f2 "$WORK/mirrors.tsv" | xargs -d '\n' -n1 dirname | sort -u | xargs -d '\n' mkdir -p
echo "mirroring $REPOS repositories"
tr '\t' '\n' < "$WORK/mirrors.tsv" | xargs -d '\n' -n2 -P "$(nproc)" git clone -q --mirror
if [ "${COPY_COMMIT_LOG:-0}" = 1 ]; then
  test "$REPOS" = 1 || { echo "COPY_COMMIT_LOG needs a single repository" >&2; exit 2; }
  MIRROR=$(cut -f2 "$WORK/mirrors.tsv")
  SOURCE_LOG="$(git -C "$SOURCE" rev-parse --absolute-git-dir)/sourcegull/commits.log"
  test -f "$SOURCE_LOG" || { echo "missing commit log: $SOURCE_LOG" >&2; exit 1; }
  mkdir -p "$MIRROR/sourcegull"
  cp "$SOURCE_LOG" "$MIRROR/sourcegull/commits.log"
fi
printf '{"auth.public": true, "disableAutoGitUpdates": true}\n' > "$WORK/site-config.jsonc"

STARTED=$(now)
"$BIN" run --roles=all --data-dir "$WORK/data" \
  --site-config-file "$WORK/site-config.jsonc" \
  --http-addr "127.0.0.1:$PORT" --internal-addr "127.0.0.1:$((PORT + 1))" \
  --repo-grpc-addr "127.0.0.1:$((PORT + 2))" \
  --repo-node-addr "$NODE" \
  --repo-nodes "$NODE" > "$WORK/sourcegull.log" 2>&1 &
SERVER=$!
ready=0
for _ in $(seq 1 120); do
  if curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null; then ready=1; break; fi
  sleep 0.5
done
if [ "$ready" != 1 ]; then tail -80 "$WORK/sourcegull.log"; exit 1; fi
if [ "${WAIT_INDEX:-1}" != 0 ]; then
  # Until the node has reported every repository indexed (or failed) after its builds.
  deadline=$(( $(date +%s) + ${INDEX_TIMEOUT:-7200} ))
  last=
  while :; do
    state=$(psql_ "SELECT count(*) FILTER (WHERE index_status = 'indexed') || ' ' ||
      count(*) FILTER (WHERE last_error IS NOT NULL) FROM search_index_repos")
    read -r indexed failed <<< "$state"
    if [ $((indexed + failed)) -ge "$REPOS" ]; then break; fi
    if [ "$(date +%s)" -ge "$deadline" ]; then
      tail -80 "$WORK/sourcegull.log"; echo "indexing timed out: $indexed indexed, $failed failed of $REPOS" >&2; exit 1
    fi
    if [ "$state" != "$last" ]; then echo "indexed $indexed/$REPOS, failed $failed ($(elapsed "$STARTED")s)"; last=$state; fi
    sleep 1
  done
  INDEX_SECS=$(elapsed "$STARTED")
  if [ "$failed" != 0 ]; then
    psql_ "SELECT repo_id, last_error FROM search_index_repos WHERE last_error IS NOT NULL LIMIT 20"
    echo "$failed repositories failed to index" >&2; exit 1
  fi
fi

if [ -n "$CORPUS" ]; then
  echo "corpus: $CORPUS ($CDIR)"
  echo "manifest sha256: $(cut -d' ' -f1 "$CDIR/manifest.sha256")"
  echo "repositories: $REPOS"
  echo "tracked bytes: $(awk -F'\t' '{sum += $5} END {printf "%.0f\n", sum}' "$CDIR/manifest.tsv")"
else
  echo "corpus: $SOURCE"
  echo "revision: $(git -C "$SOURCE" rev-parse HEAD)"
  echo "tracked bytes: $(git -C "$SOURCE" ls-tree -rl HEAD | awk '{sum += $4} END {print sum+0}')"
fi
if [ "${WAIT_INDEX:-1}" != 0 ]; then
  echo "indexed in: ${INDEX_SECS}s (server start to the last repository reported indexed)"
  echo "index bytes: $(psql_ "SELECT sum(index_bytes) FROM search_index_repos")"
fi
if [ -n "${BENCH_HOOK:-}" ]; then
  awk -F'\t' -v data="$WORK/data/repos" '{printf "%s\t%s\t%s/%02x/%s.git\n", $1, $2, data, $1 % 256, $1}' \
    "$WORK/ids.tsv" | sort -n > "$WORK/repos.tsv"
  if [ -n "$CORPUS" ]; then
    BENCH_TRACKED_BYTES=$(awk -F'\t' '{sum += $5} END {printf "%.0f\n", sum}' "$CDIR/manifest.tsv")
  else
    BENCH_TRACKED_BYTES=$(git -C "$SOURCE" ls-tree -rl HEAD | awk '{sum += $4} END {print sum+0}')
  fi
  BENCH_BASE="http://127.0.0.1:$PORT" BENCH_WORK=$WORK BENCH_DATA=$WORK/data BENCH_SERVER_PID=$SERVER \
    BENCH_INDEX_SECS=${INDEX_SECS:-} BENCH_REPOS=$WORK/repos.tsv BENCH_TRACKED_BYTES=$BENCH_TRACKED_BYTES \
    sh -c "$BENCH_HOOK"
  exit
fi
QUERY_ARGS=()
if [ -n "$CORPUS" ] && [[ " $* " != *" --queries"* ]]; then
  QUERY_ARGS=(--queries "$CDIR/queries.json")
fi
if [ -n "$CORPUS" ]; then
  while IFS= read -r arg; do QUERY_ARGS+=("$arg"); done < <(python3 -c \
    'import json, sys; print("\n".join(json.load(open(sys.argv[1])).get("stream_args", [])))' \
    "$CDIR/corpus.json" | grep .)
fi
python3 scripts/bench/search-stream.py --base "http://127.0.0.1:$PORT" --output "$OUT" \
  ${QUERY_ARGS[@]+"${QUERY_ARGS[@]}"} "$@"
echo "results: $OUT"
