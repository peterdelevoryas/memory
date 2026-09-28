# Memory service

A shared memory database for all my AI tools: Claude Code, Codex, claude.ai (web and mobile), ChatGPT, and my own agent. Agents **search** it when they need something; nothing is loaded wholesale into context. Some agents can also **consolidate** memories over time.

## Shape

- A **remote MCP server** in Rust, using the [`rmcp`](https://crates.io/crates/rmcp) crate. Every surface already speaks MCP, so this is one integration, not seven.
- Storage: **embedded [Turso](https://github.com/tursodatabase/turso)** (the `turso` crate). Pin an exact version; it's pre-1.0 (0.7.2 stable, 0.8 in pre-release).
- Runs on its **own VM**, independent of the Withings MCP server and of the agent's VM (which will run `bash`). The data lives on a network-attached block-storage volume, so the VM is disposable. Caddy terminates TLS for its own subdomain; nightly backups go to separate storage over rsync+SSH.

## Data model

Memory mirrors Claude Code's file-based memory: a set of **named notes**, each with a one-line description, plus a generated index (the equivalent of `MEMORY.md`).

```sql
CREATE TABLE notes (
  name        TEXT PRIMARY KEY,  -- slug
  version     TEXT NOT NULL,     -- new UUIDv7 on every write; used for compare-and-swap
  description TEXT NOT NULL,     -- one line, shown in the index
  text        TEXT NOT NULL,     -- the note body
  kind        TEXT,              -- user | feedback | project | reference
  tags        TEXT,              -- comma-separated
  scope       TEXT NOT NULL,     -- personal | work | health | agent
  source      TEXT NOT NULL,     -- which client last wrote it (from the token)
  origin      TEXT NOT NULL,     -- user_stated | agent_inferred | external, declared by the writer
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL
);
CREATE INDEX notes_fts ON notes USING fts (name, description, text);
```

Rules:
- **Notes are edited in place and forgetting deletes.** There is no history: memory is fixed forward, and nightly backups are the safety net. (The first design kept every version in a `supersessions` table; it was dropped on 2026-09-28 because nothing used the history, `memory_write` covered merges, and "forget" should really forget personal data.)
- Writes are **compare-and-swap**: revising or forgetting a note takes the version you read and fails if it changed since.
- **The index** is generated deterministically (no model) from notes' names and descriptions, grouped by scope, and served by the `memory_index` tool, which agents call at the start of a task. It's deliberately not in the server's connection instructions: those are frozen at connect time, land in the system prompt with elevated trust, and Claude Code truncates them at 2048 characters. Only `user_stated` and `agent_inferred` notes are listed; external content is reachable through `memory_list` and `memory_search`.

## MCP tools

| Tool | What it does | Permission |
|---|---|---|
| `memory_index` | The generated index: every note's name and description | read |
| `memory_read` | A note by name, with its current version | read |
| `memory_search` | Full-text search over names, descriptions, and bodies | read |
| `memory_list` | Every note, paginated by name; with `since`, only notes written or revised since then | read |
| `memory_write` | Create a note, or replace one given the version you read; `source` comes from the token | add |
| `memory_forget` | Delete a note given the version you read | consolidate |

To merge two notes, revise one with the combined content and forget the other.

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

**Consolidation:** the server stays inference-free; all judgment happens in agents that talk to it over MCP. A nightly claude.ai scheduled task (via the Memory connector, so it writes as `claude-ai`) follows [`consolidator.md`](consolidator.md): it reads everything with `memory_list`, merges overlaps, resolves contradictions, rewrites stale dated facts, and tightens descriptions, at most 15 changes per run, then reports what it did. Changes apply directly; the nightly backups are the way back.

**Later:** embeddings, if keyword search starts missing things.

**Before starting**, spend an hour on prior art: mem0, Letta, Zep/Graphiti, basic-memory, and the MCP reference "memory" server.
