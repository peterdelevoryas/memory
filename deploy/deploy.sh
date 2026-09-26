#!/bin/bash
# Build on the VM and (re)install. Usage: deploy/deploy.sh [host]
set -euo pipefail
HOST=${1:-root@memory.example.com}
cd "$(dirname "$0")/.."
ssh "$HOST" mkdir -p /root/src/memory
rsync -az --delete --exclude target --exclude .git --exclude '*.db*' --exclude /tokens ./ "$HOST:/root/src/memory/"
ssh "$HOST" bash -se <<'REMOTE'
set -euo pipefail
cd /root/src/memory
~/.cargo/bin/cargo build --release --locked
install -m 0755 target/release/memory /usr/local/bin/memory
install -m 0755 deploy/backup.sh /usr/local/bin/memory-backup
install -m 0644 deploy/memory.service deploy/memory-backup.service deploy/memory-backup.timer /etc/systemd/system/
install -m 0644 deploy/Caddyfile /etc/caddy/Caddyfile
touch /etc/memory/tokens && chown root:memory /etc/memory/tokens && chmod 0640 /etc/memory/tokens
systemctl daemon-reload
systemctl enable --now memory memory-backup.timer
systemctl restart memory
systemctl reload caddy
systemctl --no-pager --lines=5 status memory
REMOTE
