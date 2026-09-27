#!/bin/bash
# Inspect the production database. Snapshots it (stopping the server for about
# a second so the file and its WAL are consistent), then opens the snapshot
# in tursodb. Changes only touch the snapshot, which is deleted on exit.
#
# Usage:
#   deploy/inspect.sh                  interactive shell
#   deploy/inspect.sh "SELECT ..."     run one query and exit
#
# sqlite3 can't open the file (it doesn't understand the FTS index), so this
# uses tursodb pinned to the same version as the turso crate.
set -euo pipefail
HOST=${MEMORY_HOST:-root@memory.example.com}
TURSO_VERSION=0.7.2  # keep in sync with Cargo.toml

sql_b64=$(printf '%s' "${1:-}" | base64 | tr -d '\n')

tty=-T; [ -t 0 ] && tty=-t  # a pty only for the interactive shell
ssh "$tty" "$HOST" "TURSO_VERSION=$TURSO_VERSION SQL_B64=$sql_b64 bash -c '$(cat <<'REMOTE'
set -euo pipefail
T=/root/.turso/tursodb
if ! [ -x $T ] || ! $T --version | grep -qF "$TURSO_VERSION"; then
  echo "installing tursodb $TURSO_VERSION..." >&2
  curl --proto =https --tlsv1.2 -LsSf "https://github.com/tursodatabase/turso/releases/download/v$TURSO_VERSION/turso_cli-installer.sh" | sh >/dev/null 2>&1
fi
snap=$(mktemp -d)
trap "rm -rf $snap" EXIT
systemctl stop memory
cp -a /var/lib/memory/. "$snap/" || { systemctl start memory; exit 1; }
systemctl start memory
sql=$(printf %s "$SQL_B64" | base64 -d)
if [ -n "$sql" ]; then
  $T --experimental-index-method "$snap/memory.db" "$sql"
else
  echo "snapshot of /var/lib/memory (changes are discarded on exit)" >&2
  $T --experimental-index-method "$snap/memory.db"
fi
REMOTE
)'"
