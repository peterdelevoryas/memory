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

Runs on any Linux server you can SSH into as root (set up with `deploy/cloud-init.yaml`, written for Ubuntu 24.04).
Its hostname lives in `deploy/config` (untracked; copy `deploy/config.example`), which the deploy scripts read; below it's `memory.example.com`.

- Deploy: `deploy/deploy.sh` (rsyncs source, builds on the VM, installs, restarts).
- Tokens: `deploy/token.sh <source> <read|add|consolidate>` mints a token on the server (which stores only its
  hash), restarts it, and saves the token in the macOS keychain (`memory-mcp-token` / `<source>`). Revoke by
  deleting the line from `/etc/memory/tokens` and restarting.
- Inspect: `deploy/inspect.sh` opens a fresh snapshot of the production DB in `tursodb` (plain `sqlite3` can't read
  the FTS index); `deploy/inspect.sh "SELECT ..."` runs one query. The server stops for about a second while it copies.
- Backups: `memory-backup.timer` runs nightly at 03:30 UTC. It stops the server for a few seconds, then pushes a
  tarball of `/var/lib/memory` over rsync+SSH to `BACKUP_TARGET` (`user@host`, and optionally `BACKUP_SSH_PORT`,
  set in `/etc/memory/backup.env` on the server, with the key in `/etc/memory/backup_key`), into `backups/`, keeping 30 days.
  To restore, stop `memory`, extract the tarball's `db/` into `/var/lib/memory/`, `chown -R memory:memory`, and start it.

## Connecting clients

Every client authenticates the same way: its own bearer token, sent as an
`Authorization: Bearer <token>` header. Mint one per client, named for the
client (that name becomes the `source` on everything it writes):

```sh
deploy/token.sh claude-ai consolidate   # or read / add
security find-generic-password -s memory-mcp-token -a claude-ai -w | pbcopy
```

### Claude Code

```sh
claude mcp add --scope user --transport http memory https://memory.example.com/mcp \
  --header "Authorization: Bearer <token>"
```

### claude.ai

No OAuth needed: claude.ai custom connectors can send request headers.

1. In claude.ai, open Settings → Connectors and add a custom connector.
2. URL: `https://memory.example.com/mcp`
3. Add a request header named `Authorization` with the value `Bearer <token>`
   (the word `Bearer`, a space, then the token).

To make it the only memory, turn off claude.ai's own memory in Settings → Memory.

### Codex

In `~/.codex/config.toml`:

```toml
[mcp_servers.memory]
url = "https://memory.example.com/mcp"
http_headers = { "Authorization" = "Bearer <token>" }
```

A static header (rather than `bearer_token_env_var`) also works for Codex
inside the ChatGPT desktop app, which reads the same config but not your
shell's environment.
