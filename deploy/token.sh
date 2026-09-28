#!/bin/bash
# Mint a client token on the production server and store it in the macOS
# keychain (service "memory-mcp-token", account = source). The server keeps
# only the token's hash. Re-running for the same source adds a second token;
# remove the old line from /etc/memory/tokens to revoke it.
#
# Usage: deploy/token.sh <source> <read|add|consolidate>
# Read it back: security find-generic-password -s memory-mcp-token -a <source> -w
set -euo pipefail
. "$(dirname "$0")/lib.sh"
[ $# -eq 2 ] || { sed -n 's/^# \{0,1\}//; 2,8p' "$0" >&2; exit 2; }
source=$1 level=$2

token=$(ssh "$HOST" bash -s -- "$source" "$level" <<'REMOTE'
set -euo pipefail
err=$(mktemp); trap 'rm -f $err' EXIT
line=$(memory token "$1" "$2" 2>"$err") || { cat "$err" >&2; exit 1; }
echo "$line" >> /etc/memory/tokens
systemctl reload memory
sed -n 2p "$err"
REMOTE
)
security add-generic-password -U -s memory-mcp-token -a "$source" -w "$token"
echo "minted $level token for $source; stored in keychain (memory-mcp-token / $source)" >&2
