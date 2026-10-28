#!/usr/bin/env bash
# Start a throwaway local Postgres for development and tests.
# Usage: scripts/dev-postgres.sh [datadir]   (prints the DATABASE_URL to export)
set -euo pipefail
PGBIN="${PGBIN:-$(ls -d /usr/lib/postgresql/*/bin 2>/dev/null | sort -V | tail -1)}"
DATA="${1:-${TMPDIR:-/tmp}/sourcegull-pg}"
PORT="${PGPORT:-55432}"
# Postgres refuses to run as root; drop to the postgres user when needed.
AS=()
if [ "$(id -u)" = 0 ]; then AS=(runuser -u postgres --); fi
if [ ! -f "$DATA/PG_VERSION" ]; then
  mkdir -p "$DATA"
  if [ "$(id -u)" = 0 ]; then chown postgres "$DATA"; fi
  "${AS[@]}" "$PGBIN/initdb" -D "$DATA" -U sourcegull --auth=trust >/dev/null
fi
if ! "${AS[@]}" "$PGBIN/pg_ctl" -D "$DATA" status >/dev/null 2>&1; then
  "${AS[@]}" "$PGBIN/pg_ctl" -D "$DATA" -o "-p $PORT -k $DATA -c listen_addresses=127.0.0.1" -l "$DATA/log" -w start >/dev/null
fi
"$PGBIN/createdb" -h 127.0.0.1 -p "$PORT" -U sourcegull sourcegull 2>/dev/null || true
echo "postgres://sourcegull@127.0.0.1:$PORT/sourcegull"
