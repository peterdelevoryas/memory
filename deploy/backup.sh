#!/bin/bash
# Nightly backup to another machine over rsync+SSH. The server is stopped for the copy so the
# database file and its WAL are consistent (the FTS index lives in the same
# file); downtime is a few seconds.
set -euo pipefail
. /etc/memory/backup.env   # BACKUP_TARGET=user@host, optionally BACKUP_SSH_PORT=22
KEEP_DAYS=30
SSH=(ssh -p "${BACKUP_SSH_PORT:-22}" -i /etc/memory/backup_key -o BatchMode=yes)
stamp=$(date -u +%Y-%m-%dT%H%MZ)
work=$(mktemp -d)
trap 'rm -rf "$work"; systemctl start memory' EXIT

systemctl stop memory
mkdir "$work/db"
cp -a /var/lib/memory/. "$work/db/"
systemctl start memory

tar -C "$work" -czf "$work/memory-$stamp.tar.gz" db
rsync -e "${SSH[*]}" "$work/memory-$stamp.tar.gz" "$BACKUP_TARGET:backups/"

# Prune old backups.
cutoff=$(date -u -d "-$KEEP_DAYS days" +%Y-%m-%d)
"${SSH[@]}" "$BACKUP_TARGET" ls backups | while read -r f; do
  if [[ $f =~ ^memory-([0-9]{4}-[0-9]{2}-[0-9]{2}) ]] && [[ ${BASH_REMATCH[1]} < $cutoff ]]; then
    "${SSH[@]}" "$BACKUP_TARGET" rm "backups/$f"
  fi
done
# Tell the server when the last backup succeeded; GET /health reports its age.
date -u +%Y-%m-%dT%H:%M:%SZ > /var/lib/memory/last-backup
chown memory:memory /var/lib/memory/last-backup
echo "backed up memory-$stamp.tar.gz"
