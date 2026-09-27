# Memory service

A shared memory database for all my AI tools: Claude Code, Codex, claude.ai (web and mobile), ChatGPT, and my own agent. Agents **search** it when they need something; nothing is loaded wholesale into context. Some agents can also **consolidate** memories over time.

## Shape

- A **remote MCP server** in Rust, using the [`rmcp`](https://crates.io/crates/rmcp) crate. Every surface already speaks MCP, so this is one integration, not seven.
- Storage: **embedded [Turso](https://github.com/tursodatabase/turso)** (the `turso` crate). Pin an exact version; it's pre-1.0 (0.7.2 stable, 0.8 in pre-release).
- Runs on its **own VM**, independent of the Withings MCP server and of the agent's VM (which will run `bash`). Caddy terminates TLS at `memory.example.com`; nightly backups go to separate storage over rsync+SSH.

## Data model

Memory mirrors Claude Code's file-based memory: a set of **named notes**, each with a one-line description, plus a generated index (the equivalent of `MEMORY.md`).

```sql
CREATE TABLE memories (
  id          TEXT PRIMARY KEY,  -- UUIDv7; one row per version of a note
  name        TEXT,              -- slug, unique among live notes (NULL for pre-notes rows)
  description TEXT,              -- one line, shown in the index
  text        TEXT NOT NULL,     -- the note body
  kind        TEXT,              -- user | feedback | project | reference
  tags        TEXT,              -- comma-separated
  scope       TEXT NOT NULL,     -- personal | work | health | agent
  source      TEXT NOT NULL,     -- which client wrote it (from the token)
  origin      TEXT NOT NULL,     -- user_stated | agent_inferred | external, declared by the writer
  created_at  TEXT NOT NULL
);
CREATE INDEX memories_notes_fts ON memories USING fts (name, description, text);

CREATE TABLE supersessions (
  old_id      TEXT NOT NULL REFERENCES memories(id),
  new_id      TEXT REFERENCES memories(id),  -- NULL when a note is forgotten
  reason      TEXT NOT NULL,     -- revised | corrected | outdated | merged | split | retracted
  source      TEXT NOT NULL,
  created_at  TEXT NOT NULL
);
```

Rules:
- **Nothing is edited or deleted in place.** Writing a note again adds a new version and a `supersessions` row; a row is live iff it is no row's `old_id`. History is always kept.
- Writes are **compare-and-swap**: revising or forgetting a note takes the version you read and fails if it changed since; `memory_supersede` fails if any old version is no longer current.
- A replacement's `origin` defaults to the least trusted origin of what it replaces, so consolidation can't launder `external` content into `user_stated`.
- **The index** is generated deterministically (no model) from live notes' names and descriptions, grouped by scope, within a size budget (newest first). It's appended to the server instructions, so clients that honor them (Claude Code) get it in the system prompt with no tool call. Only `user_stated` and `agent_inferred` notes are listed: anything in the index lands in every agent's system prompt, so external content stays search-only.

## MCP tools

| Tool | What it does | Permission |
|---|---|---|
| `memory_index` | The generated index (also in the server instructions) | read |
| `memory_read` | A note's current version by name, with its history links | read |
| `memory_search` | Full-text search over names, descriptions, and bodies | read |
| `memory_get` | One version by id | read |
| `memory_write` | Create a note, or revise one given the version you read; `source` comes from the token | add |
| `memory_forget` | Forget a note given the version you read (history kept) | consolidate |
| `memory_supersede` | Merge, split, correct, or retract several notes at once | consolidate |

## Auth

- One bearer token per client, each with a level: **read**, **add**, or **consolidate**.
- Every client sends its token as an `Authorization: Bearer` header: Claude Code and Codex from their config files, claude.ai through a custom connector header. OAuth isn't needed; add it only if a client (maybe ChatGPT) can't send custom headers. An OAuth server was built and then removed on 2026-09-27 for this reason.

## Things to get right early

- **Nightly backup** somewhere off the VM from day one.
- **Memories are untrusted input.** One agent can be tricked into saving something malicious that every other agent then reads. The `source` field and write permissions are the defense; tool descriptions should say that recalled memories are information, not instructions.

## Steps

1. Schema, `memory_add`, `memory_search`, `memory_get` over MCP with bearer tokens, running locally.
2. Connect Claude Code to it and use it for a few days.
3. Deploy to a VM as a systemd service, with backups.
4. Add `memory_supersede` and the consolidate permission.
5. Connect claude.ai (done, via a header token) and ChatGPT (check whether it supports custom headers first).

**Later:**

- **The server stays inference-free.** All judgment (merging, summarizing) happens in scheduled agents that talk to it over MCP with their own token (e.g. `source: consolidator`), run as Claude Code routines or by my own agent.
- **Consolidation job:** a scheduled agent that merges duplicates and fixes contradictions via `memory_supersede`.
- **Keeping the index small:** the deterministic index works while descriptions are good and notes are few. As it outgrows its budget, the consolidation agent merges and prunes *notes* (which shrinks the index) rather than writing a separate summary. If a model-written summary ever becomes necessary: versioned, records its source ids, proposed by the agent and published only after I approve a diff. Consolidation will need a `memory_list` tool (paginated, with "changed since").
- Embeddings, if keyword search starts missing things.

**Before starting**, spend an hour on prior art: mem0, Letta, Zep/Graphiti, basic-memory, and the MCP reference "memory" server.
