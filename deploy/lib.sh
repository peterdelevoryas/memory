# Sourced by the deploy scripts: loads deploy/config and sets HOST.
config="$(dirname "${BASH_SOURCE[0]}")/config"
[ -f "$config" ] || { echo "missing $config; copy deploy/config.example and set MEMORY_DOMAIN" >&2; exit 1; }
. "$config"
HOST=${MEMORY_HOST:-root@$MEMORY_DOMAIN}
