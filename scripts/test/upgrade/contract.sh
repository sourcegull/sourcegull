#!/usr/bin/env bash
# Schema contract check (used by scripts/test/upgrade.sh): the migrations of this checkout must
# not remove a column, table or allowed value that the previous release still uses, since the
# previous release keeps running on the new schema during a rolling upgrade.
#
#   contract.sh PREV_REF
#
# Applies the previous release's main migrations and this checkout's to two scratch databases,
# lists the columns of each and the values allowed by CHECK constraints (`col IN (...)`) and
# enum types, and fails for each one that is gone in this checkout while a source file of the
# previous release (crates/, without the migrations and schema snapshot) still names it together with its table.
# A removal that is safe although named (only in a comment, say) is listed in
# contract-allow.txt next to this script, one `column TABLE.COLUMN` or
# `value TABLE.COLUMN VALUE` line each.
#
# Environment: CONTRACT_PSQL, the psql command for a superuser connection to a scratch server
# (word-split; default `psql`, with the server from the usual PG* variables).
set -euo pipefail

PREV=${1:?previous release ref}
cd "$(dirname "$0")/../../.."
read -ra PSQL <<<"${CONTRACT_PSQL:-psql}"
ALLOW=scripts/test/upgrade/contract-allow.txt
MIGRATIONS=crates/db/migrations/main
WORK=$(mktemp -d)
DBS=(sourcegull_contract_prev sourcegull_contract_next)
cleanup() {
  for db in "${DBS[@]}"; do "${PSQL[@]}" -qc "DROP DATABASE IF EXISTS $db" >/dev/null 2>&1 || true; done
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

# Columns, CHECK constraint values and enum values, one per line.
SCHEMA_SQL=$(cat <<'EOF'
SELECT 'column ' || table_name || '.' || column_name
  FROM information_schema.columns WHERE table_schema = 'public'
UNION
SELECT 'value ' || c.conrelid::regclass || '.' || m[1] || ' ' || v[1]
  FROM pg_constraint c,
       regexp_matches(pg_get_constraintdef(c.oid),
         '(\w+)\)?(?:::\w+)? = (?:ANY \(\(?ARRAY\[([^]]*)\]|(''[^'']*''))', 'g') m,
       regexp_matches(coalesce(m[2], m[3]), '''([^'']*)''', 'g') v
 WHERE c.contype = 'c' AND c.connamespace = 'public'::regnamespace
UNION
SELECT 'value ' || t.typname || ' ' || e.enumlabel
  FROM pg_enum e JOIN pg_type t ON t.oid = e.enumtypid
ORDER BY 1
EOF
)

# schema DB DIR: applies DIR's up migrations to a new database DB; prints its schema lines.
schema() {
  local db=$1 dir=$2 f
  "${PSQL[@]}" -qc "DROP DATABASE IF EXISTS $db" >/dev/null 2>&1
  "${PSQL[@]}" -qc "CREATE DATABASE $db" >/dev/null
  for f in $(find "$dir" -name '*.up.sql' | sort); do
    "${PSQL[@]}" -q -v ON_ERROR_STOP=1 -d "$db" <"$f" >/dev/null || fail "migration $(basename "$f") failed"
  done
  "${PSQL[@]}" -tA -d "$db" -c "$SCHEMA_SQL"
}

mkdir -p "$WORK/prev"
git archive "$PREV" "$MIGRATIONS" | tar -x -C "$WORK/prev"
schema "${DBS[0]}" "$WORK/prev/$MIGRATIONS" >"$WORK/prev.txt"
schema "${DBS[1]}" "$MIGRATIONS" >"$WORK/next.txt"
comm -23 "$WORK/prev.txt" "$WORK/next.txt" >"$WORK/removed.txt"
echo "previous release $(git log -1 --format=%h "$PREV"): $(wc -l <"$WORK/prev.txt") schema items, $(wc -l <"$WORK/removed.txt") removed"

# Source files of the previous release that name every word given.
named_in() {
  local args=() w
  for w in "$@"; do args+=(-e "$w"); done
  git grep -l -w --all-match "${args[@]}" "$PREV" -- crates ':!crates/db/migrations' ':!crates/db/schema' 2>/dev/null |
    sed "s#^$PREV:##" || true
}

bad=0
while read -r kind item value; do
  [ -n "$kind" ] || continue
  line="$kind $item${value:+ $value}"
  if [ -f "$ALLOW" ] && grep -qxF "$line" "$ALLOW"; then
    echo "allowed: $line"
    continue
  fi
  table=${item%%.*} column=${item#*.}
  if [ "$kind" = column ]; then
    files=$(named_in "$table" "$column")
  elif [ "$table" = "$item" ]; then
    files=$(named_in "$value") # an enum type: TYPE VALUE
  else
    files=$(named_in "$table" "$column" "$value")
  fi
  if [ -n "$files" ]; then
    echo "removed but still named by the previous release: $line ($(echo "$files" | head -n 3 | paste -sd ' '))" >&2
    bad=1
  else
    echo "removed, unused by the previous release: $line"
  fi
done <"$WORK/removed.txt"
[ "$bad" = 0 ] || fail "a migration removes what the previous release uses: keep it until the release after the code stops using it (docs/development.md#migrations)"
echo "OK: no column, table or value the previous release uses is removed"
