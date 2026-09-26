# memory

A shared memory MCP server for AI tools. See [PLAN.md](PLAN.md) for the design.

## Run locally

```sh
cargo build --release
# Mint a token per client; the tokens file stores only hashes.
./target/release/memory token claude-code add >> tokens
./target/release/memory serve        # http://127.0.0.1:8750/mcp
```

Environment: `MEMORY_DB` (default `memory.db`), `MEMORY_TOKENS` (default `tokens`),
`MEMORY_ADDR` (default `127.0.0.1:8750`), `MEMORY_ALLOWED_HOSTS` (default localhost).

## Production

Runs at `https://memory.example.com/mcp` on its own VM.

- Deploy: `deploy/deploy.sh root@memory.example.com` (rsyncs source, builds on the VM, installs, restarts).
- Tokens: `ssh root@memory.example.com 'memory token <source> <level> >> /etc/memory/tokens && systemctl restart memory'`
  (the token is printed to stderr; only its hash is stored).
- Backups: `memory-backup.timer` runs nightly at 03:30 UTC. It stops the server for a few seconds, then pushes a
  tarball of `/var/lib/memory` over rsync+SSH to `BACKUP_TARGET` (`user@host`, and optionally `BACKUP_SSH_PORT`,
  set in `/etc/memory/backup.env` on the server, with the key in `/etc/memory/backup_key`), into `backups/`, keeping 30 days.
  To restore, stop `memory`, extract the tarball's `db/` into `/var/lib/memory/`, `chown -R memory:memory`, and start it.

## Connect Claude Code

```sh
claude mcp add --scope user --transport http memory https://memory.example.com/mcp \
  --header "Authorization: Bearer <token>"
```
