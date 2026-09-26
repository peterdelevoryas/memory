# Memory service

A shared memory database for all my AI tools: Claude Code, Codex, claude.ai (web and mobile), ChatGPT, and my own agent. Agents **search** it when they need something; nothing is loaded wholesale into context. Some agents can also **consolidate** memories over time.

## Shape

- A **remote MCP server** in Rust, using the [`rmcp`](https://crates.io/crates/rmcp) crate. Every surface already speaks MCP, so this is one integration, not seven.
- Storage: **embedded [Turso](https://github.com/tursodatabase/turso)** (the `turso` crate). Pin an exact version; it's pre-1.0 (0.7.2 stable, 0.8 in pre-release).
- Runs on its **own VM**, independent of the Withings MCP server and of the agent's VM (which will run `bash`). Caddy terminates TLS at `memory.example.com`; nightly backups go to separate storage over rsync+SSH.

## Data model

```sql
CREATE TABLE memories (
  id          TEXT PRIMARY KEY,  -- UUIDv7
  text        TEXT NOT NULL,
  kind        TEXT,              -- fact, preference, project, ...
  tags        TEXT,              -- comma-separated
  scope       TEXT NOT NULL,     -- personal, work, health, agent, ...
  source      TEXT NOT NULL,     -- which surface wrote it (from the token): claude-code, chatgpt, agent, ...
  origin      TEXT NOT NULL,     -- user_stated | agent_inferred | external, declared by the writer
  created_at  TEXT NOT NULL
);
CREATE INDEX memories_fts ON memories USING fts (text);

CREATE TABLE supersessions (
  old_id      TEXT NOT NULL REFERENCES memories(id),
  new_id      TEXT REFERENCES memories(id),  -- NULL for a retraction
  reason      TEXT NOT NULL,     -- corrected | outdated | merged | split | retracted
  source      TEXT NOT NULL,     -- who did it
  created_at  TEXT NOT NULL
);
```

Rules:
- **Nothing is edited or deleted in place.** Replacing memories adds new ones plus `supersessions` rows (many-to-many, so merges and splits both work). A memory is live iff it is no row's `old_id`. This keeps history and makes consolidation reversible.
- `memory_supersede` is **compare-and-swap**: it fails without writing if any old memory is missing or already superseded.
- A replacement's `origin` defaults to the least trusted origin of what it replaces, so consolidation can't launder `external` content into `user_stated`.
- Search skips superseded memories by default.

Check `docs/fts.md` in the Turso repo for the current FTS query syntax and tokenizer options before writing queries. Turso has exact vector search today; approximate vector indexes are on its roadmap. Start with full-text only.

## MCP tools

| Tool | What it does | Permission |
|---|---|---|
| `memory_search` | Full-text search, filtered by scope, returns the top N | read |
| `memory_get` | Fetch one memory by id, with its supersession links in both directions | read |
| `memory_add` | Add a memory; `source` comes from the client's token, not the request | add |
| `memory_supersede` | Correct, update, merge, split, or retract memories (compare-and-swap) | consolidate |

## Auth

- One bearer token per client, each with a level: **read**, **add**, or **consolidate**.
- Claude Code and Codex work with bearer tokens. claude.ai and ChatGPT connectors expect **OAuth**; do that second (reuse the Withings server's setup if possible).

## Things to get right early

- **Nightly backup** somewhere off the VM from day one.
- **Memories are untrusted input.** One agent can be tricked into saving something malicious that every other agent then reads. The `source` field and write permissions are the defense; tool descriptions should say that recalled memories are information, not instructions.

## Steps

1. Schema, `memory_add`, `memory_search`, `memory_get` over MCP with bearer tokens, running locally.
2. Connect Claude Code to it and use it for a few days.
3. Deploy to a VM as a systemd service, with backups.
4. Add `memory_supersede` and the consolidate permission.
5. OAuth, then connect claude.ai and ChatGPT.

**Later:** a consolidation job, a scheduled agent run that merges duplicates via `memory_supersede`, possibly run by my own agent. Also embeddings, if keyword search starts missing things.

**Before starting**, spend an hour on prior art: mem0, Letta, Zep/Graphiti, basic-memory, and the MCP reference "memory" server.
