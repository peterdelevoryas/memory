use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use turso::{Builder, Database, Value, transaction::TransactionBehavior};

// Memories are notes with a stable name, like files in a directory. They are
// never edited or deleted: writing a note again adds a new version and records
// a row in `supersessions`; a memory is live iff it appears in no row's
// `old_id`. A retraction (forgetting) is a supersession with no `new_id`.
// Among live memories, a name is unique. Rows from before notes existed have
// no name or description.
const TABLES: &str = "
CREATE TABLE IF NOT EXISTS memories (
  id          TEXT PRIMARY KEY,
  text        TEXT NOT NULL,
  kind        TEXT,
  tags        TEXT,
  scope       TEXT NOT NULL,
  source      TEXT NOT NULL,
  origin      TEXT NOT NULL,
  created_at  TEXT NOT NULL,
  name        TEXT,
  description TEXT
);
CREATE TABLE IF NOT EXISTS supersessions (
  old_id      TEXT NOT NULL REFERENCES memories(id),
  new_id      TEXT REFERENCES memories(id),
  reason      TEXT NOT NULL,
  source      TEXT NOT NULL,
  created_at  TEXT NOT NULL
);
";

const INDEXES: &str = "
CREATE INDEX IF NOT EXISTS memories_notes_fts ON memories USING fts (name, description, text)
  WITH (weights = 'name=2.0,description=2.0,text=1.0');
CREATE INDEX IF NOT EXISTS memories_name ON memories (name);
CREATE INDEX IF NOT EXISTS supersessions_old ON supersessions (old_id);
CREATE INDEX IF NOT EXISTS supersessions_new ON supersessions (new_id);
";

const COLUMNS: &str = "memories.id, memories.text, memories.kind, memories.tags, memories.scope, \
    memories.source, memories.origin, memories.created_at, \
    EXISTS (SELECT 1 FROM supersessions WHERE supersessions.old_id = memories.id), \
    memories.name, memories.description";

const LIVE: &str =
    "NOT EXISTS (SELECT 1 FROM supersessions WHERE supersessions.old_id = memories.id)";

const FTS_COLUMNS: &str = "name, description, text";

pub const MAX_NAME_CHARS: usize = 64;
pub const MAX_DESCRIPTION_CHARS: usize = 150;
pub const MAX_TEXT_CHARS: usize = 4000;

/// Where a memory's content came from, as declared by the agent that wrote it.
/// Ordered from least to most trusted.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Taken from tool output, web pages, documents, or other content the user didn't write.
    External,
    /// Inferred or concluded by an agent, not stated directly by the user.
    AgentInferred,
    /// Stated directly by the user.
    UserStated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// A new version of the same note.
    Revised,
    /// The old memory was wrong.
    Corrected,
    /// The old memory was true but no longer is.
    Outdated,
    /// Several memories were combined into one.
    Merged,
    /// One memory was broken into several.
    Split,
    /// The old memory should be forgotten, with no replacement.
    Retracted,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Memory {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub tags: Vec<String>,
    pub scope: String,
    pub source: String,
    pub origin: Origin,
    pub created_at: String,
    pub superseded: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Link {
    /// The memory on the other side of the link; absent for a retraction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub reason: Reason,
    pub source: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct MemoryDetail {
    #[serde(flatten)]
    pub memory: Memory,
    /// Memories this one replaced (e.g. the previous version of this note).
    pub supersedes: Vec<Link>,
    /// What replaced this memory, if anything.
    pub superseded_by: Vec<Link>,
}

pub struct NewMemory {
    pub name: String,
    pub description: String,
    pub text: String,
    pub kind: Option<String>,
    pub tags: Vec<String>,
    pub scope: String,
    /// For a supersession, `None` means the least trusted origin of the old memories.
    pub origin: Option<Origin>,
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("no memory with id {0}")]
    NotFound(String),
    #[error("memory {0} has already been superseded; read it again to see what replaced it")]
    AlreadySuperseded(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<turso::Error> for WriteError {
    fn from(e: turso::Error) -> Self {
        Self::Other(e.into())
    }
}

#[derive(Clone)]
pub struct Store {
    db: Database,
    /// Index lines for live notes, newest first: (scope, line).
    index: Arc<RwLock<Vec<(String, String)>>>,
}

impl Store {
    pub async fn open(path: &str) -> Result<Self> {
        let open = || async {
            Builder::new_local(path)
                .experimental_index_method(true)
                .build()
                .await
                .with_context(|| format!("opening database {path}"))
        };
        let db = open().await?;
        let conn = db.connect()?;
        conn.execute_batch(TABLES)
            .await
            .context("creating tables")?;
        migrate(&conn).await.context("migrating schema")?;
        conn.execute_batch(INDEXES)
            .await
            .context("creating indexes")?;
        drop(conn);
        let store = Self {
            db,
            index: Arc::default(),
        };
        store.refresh_index().await.context("building index")?;
        Ok(store)
    }

    fn conn(&self) -> Result<turso::Connection> {
        let conn = self.db.connect()?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(conn)
    }

    /// Creates a note, or with `expected_version` writes a new version of one.
    /// Compare-and-swap: creating fails if a live note has this name, and
    /// revising fails unless `expected_version` is the note's current version.
    pub async fn write(
        &self,
        note: NewMemory,
        expected_version: Option<String>,
        source: &str,
    ) -> Result<Memory, WriteError> {
        let origin = note
            .origin
            .ok_or_else(|| WriteError::Invalid("origin is required".into()))?;
        let memory =
            build(note, source, origin).map_err(|e| WriteError::Invalid(format!("{e:#}")))?;
        let name = memory.name.clone().unwrap_or_default();

        let mut conn = self.conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await?;
        let current = live_by_name(&tx, &name).await?;
        match (current, expected_version) {
            (None, None) => {}
            (None, Some(_)) => {
                return Err(WriteError::Conflict(format!(
                    "there is no note named {name:?} to revise (it may have been forgotten or renamed); \
                     omit expected_version to create it"
                )));
            }
            (Some(cur), None) => {
                return Err(WriteError::Conflict(format!(
                    "a note named {name:?} already exists (version {}); read it and pass \
                     expected_version to revise it, or choose another name",
                    cur.id
                )));
            }
            (Some(cur), Some(expected)) if cur.id != expected => {
                return Err(WriteError::Conflict(format!(
                    "note {name:?} changed since you read it: current version is {}, not {expected}; \
                     read it again and merge your changes",
                    cur.id
                )));
            }
            (Some(cur), Some(_)) => {
                insert(&tx, &memory).await?;
                link(&tx, &cur.id, Some(&memory.id), Reason::Revised, source).await?;
                tx.commit().await?;
                self.refresh_index().await?;
                return Ok(memory);
            }
        }
        insert(&tx, &memory).await?;
        tx.commit().await?;
        self.refresh_index().await?;
        Ok(memory)
    }

    /// Atomically replace `old_ids` with `new` memories. Compare-and-swap:
    /// fails without writing anything if any old memory is missing or already
    /// superseded, or if a new note's name is taken by a live note that isn't
    /// being replaced.
    pub async fn supersede(
        &self,
        old_ids: Vec<String>,
        new: Vec<NewMemory>,
        reason: Reason,
        source: &str,
    ) -> Result<Vec<Memory>, WriteError> {
        let mut old_ids = old_ids;
        old_ids.sort();
        old_ids.dedup();
        if old_ids.is_empty() {
            return Err(WriteError::Invalid("old_ids must not be empty".into()));
        }
        match (reason, new.is_empty()) {
            (Reason::Retracted, false) => {
                return Err(WriteError::Invalid(
                    "a retraction takes no new memories".into(),
                ));
            }
            (r, true) if r != Reason::Retracted => {
                return Err(WriteError::Invalid(
                    "new must not be empty; use reason \"retracted\" to forget without replacing"
                        .into(),
                ));
            }
            _ => {}
        }
        let mut names: Vec<&str> = new.iter().map(|n| n.name.as_str()).collect();
        names.sort();
        if names.windows(2).any(|w| w[0] == w[1]) {
            return Err(WriteError::Invalid(
                "new notes must have distinct names".into(),
            ));
        }

        let mut conn = self.conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await?;

        let mut weakest = Origin::UserStated;
        for id in &old_ids {
            let Some(old) = by_id(&tx, id).await? else {
                return Err(WriteError::NotFound(id.clone()));
            };
            if old.superseded {
                return Err(WriteError::AlreadySuperseded(id.clone()));
            }
            weakest = weakest.min(old.origin);
        }
        for n in &new {
            if let Some(cur) = live_by_name(&tx, &n.name).await?
                && !old_ids.contains(&cur.id)
            {
                return Err(WriteError::Conflict(format!(
                    "a note named {:?} already exists (version {}) and isn't being replaced",
                    n.name, cur.id
                )));
            }
        }

        let mut created = Vec::new();
        for n in new {
            let origin = n.origin.unwrap_or(weakest);
            let memory =
                build(n, source, origin).map_err(|e| WriteError::Invalid(format!("{e:#}")))?;
            insert(&tx, &memory).await?;
            created.push(memory);
        }
        let new_ids: Vec<Option<&str>> = if created.is_empty() {
            vec![None]
        } else {
            created.iter().map(|m| Some(m.id.as_str())).collect()
        };
        for old_id in &old_ids {
            for new_id in &new_ids {
                link(&tx, old_id, *new_id, reason, source).await?;
            }
        }
        tx.commit().await?;
        self.refresh_index().await?;
        Ok(created)
    }

    /// Forgets the note `name` if its current version is `expected_version`.
    pub async fn forget(
        &self,
        name: &str,
        expected_version: &str,
        source: &str,
    ) -> Result<(), WriteError> {
        let current = live_by_name(&self.conn()?, name).await?;
        match current {
            None => Err(WriteError::Conflict(format!(
                "there is no note named {name:?}"
            ))),
            Some(cur) if cur.id != expected_version => Err(WriteError::Conflict(format!(
                "note {name:?} changed since you read it: current version is {}",
                cur.id
            ))),
            // `supersede` rechecks liveness inside its transaction.
            Some(cur) => self
                .supersede(vec![cur.id], vec![], Reason::Retracted, source)
                .await
                .map(|_| ()),
        }
    }

    pub async fn get(&self, id: &str) -> Result<Option<MemoryDetail>> {
        let conn = self.conn()?;
        match by_id(&conn, id).await? {
            Some(memory) => Ok(Some(detail(&conn, memory).await?)),
            None => Ok(None),
        }
    }

    /// The current version of a note.
    pub async fn read(&self, name: &str) -> Result<Option<MemoryDetail>> {
        let conn = self.conn()?;
        match live_by_name(&conn, name).await? {
            Some(memory) => Ok(Some(detail(&conn, memory).await?)),
            None => Ok(None),
        }
    }

    /// Full-text search over names, descriptions, and bodies, best match
    /// first. `query` uses Tantivy query syntax; `scopes` empty means all.
    pub async fn search(
        &self,
        query: &str,
        scopes: &[String],
        include_superseded: bool,
        limit: u32,
    ) -> Result<Vec<Memory>> {
        let mut sql = format!(
            "SELECT {COLUMNS}, fts_score({FTS_COLUMNS}, ?1) AS score FROM memories \
             WHERE fts_match({FTS_COLUMNS}, ?1)"
        );
        let mut params = vec![Value::Text(query.to_string())];
        if !include_superseded {
            sql.push_str(&format!(" AND {LIVE}"));
        }
        if !scopes.is_empty() {
            let placeholders: Vec<String> =
                (0..scopes.len()).map(|i| format!("?{}", i + 2)).collect();
            sql.push_str(&format!(" AND scope IN ({})", placeholders.join(", ")));
            params.extend(scopes.iter().map(|s| Value::Text(s.clone())));
        }
        sql.push_str(&format!(" ORDER BY score DESC LIMIT {limit}"));

        let mut rows = self.conn()?.query(sql, params).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row_to_memory(&row)?);
        }
        Ok(out)
    }

    /// The index of live notes as of the last write, at most `budget`
    /// characters: one line per note, grouped by scope. Only notes stated by
    /// the user or inferred by an agent are listed (external content stays
    /// searchable but never lands in an agent's system prompt). Over budget,
    /// the most recently written notes win.
    pub fn index(&self, budget: usize) -> String {
        const OMITTED_RESERVE: usize = 80;
        let entries = self.index.read().unwrap();
        let mut kept: Vec<&(String, String)> = Vec::new();
        let mut scopes: Vec<&str> = Vec::new();
        let mut used = 0;
        for (i, entry) in entries.iter().enumerate() {
            let (scope, line) = entry;
            let header = if scopes.contains(&scope.as_str()) {
                0
            } else {
                scope.len() + 6
            };
            let reserve = if i + 1 < entries.len() {
                OMITTED_RESERVE
            } else {
                0
            };
            if used + header + line.len() + 1 + reserve > budget {
                break;
            }
            used += header + line.len() + 1;
            if header > 0 {
                scopes.push(scope);
            }
            kept.push(entry);
        }
        let omitted = entries.len() - kept.len();
        kept.sort();
        let mut out = String::new();
        let mut current = None;
        for (scope, line) in kept {
            if current != Some(scope) {
                out.push_str(&format!("\n### {scope}\n"));
                current = Some(scope);
            }
            out.push_str(line);
            out.push('\n');
        }
        if omitted > 0 {
            out.push_str(&format!(
                "\n({omitted} older notes not listed: call memory_index.)\n"
            ));
        }
        out.trim().to_string()
    }

    async fn refresh_index(&self) -> Result<()> {
        let mut rows = self
            .conn()?
            .query(
                format!(
                    "SELECT name, description, kind, scope FROM memories \
                     WHERE name IS NOT NULL AND origin IN ('user_stated', 'agent_inferred') AND {LIVE} \
                     ORDER BY created_at DESC"
                ),
                (),
            )
            .await?;
        let mut entries = Vec::new();
        while let Some(row) = rows.next().await? {
            let name = required(&row, 0)?;
            let description = text(&row, 1)?.unwrap_or_default();
            let kind = text(&row, 2)?
                .map(|k| format!(" ({k})"))
                .unwrap_or_default();
            entries.push((required(&row, 3)?, format!("- {name}: {description}{kind}")));
        }
        *self.index.write().unwrap() = entries;
        Ok(())
    }
}

/// Upgrades databases created before notes had names by rebuilding the table.
/// (In turso 0.7.2, columns added with ALTER TABLE stay invisible to the
/// database's other connections, so the migration can't use it.)
async fn migrate(conn: &turso::Connection) -> Result<()> {
    let mut rows = conn
        .query("SELECT name FROM pragma_table_info('memories')", ())
        .await?;
    let mut has_name = false;
    while let Some(row) = rows.next().await? {
        has_name |= text(&row, 0)?.as_deref() == Some("name");
    }
    drop(rows);
    if has_name {
        return Ok(());
    }
    tracing::info!("migrating memories table: adding name and description");
    let create_new = TABLES.split(';').next().unwrap().replace(
        "CREATE TABLE IF NOT EXISTS memories",
        "CREATE TABLE memories_new",
    );
    conn.execute_batch(&format!(
        "BEGIN IMMEDIATE;
         {create_new};
         INSERT INTO memories_new (id, text, kind, tags, scope, source, origin, created_at)
           SELECT id, text, kind, tags, scope, source, origin, created_at FROM memories;
         DROP INDEX IF EXISTS memories_fts;
         DROP TABLE memories;
         ALTER TABLE memories_new RENAME TO memories;
         COMMIT;"
    ))
    .await?;
    Ok(())
}

async fn by_id(conn: &turso::Connection, id: &str) -> Result<Option<Memory>> {
    let mut rows = conn
        .query(
            format!("SELECT {COLUMNS} FROM memories WHERE memories.id = ?1"),
            vec![Value::Text(id.to_string())],
        )
        .await?;
    rows.next()
        .await?
        .map(|row| row_to_memory(&row))
        .transpose()
}

async fn live_by_name(conn: &turso::Connection, name: &str) -> Result<Option<Memory>> {
    let mut rows = conn
        .query(
            format!("SELECT {COLUMNS} FROM memories WHERE memories.name = ?1 AND {LIVE}"),
            vec![Value::Text(name.to_string())],
        )
        .await?;
    rows.next()
        .await?
        .map(|row| row_to_memory(&row))
        .transpose()
}

async fn detail(conn: &turso::Connection, memory: Memory) -> Result<MemoryDetail> {
    let supersedes = links(conn, "new_id", "old_id", &memory.id).await?;
    let superseded_by = links(conn, "old_id", "new_id", &memory.id).await?;
    Ok(MemoryDetail {
        memory,
        supersedes,
        superseded_by,
    })
}

/// Links where `id` is in column `this`, reporting the memory in column `other`.
async fn links(conn: &turso::Connection, this: &str, other: &str, id: &str) -> Result<Vec<Link>> {
    let mut rows = conn
        .query(
            format!(
                "SELECT {other}, reason, source, created_at FROM supersessions
                 WHERE {this} = ?1 ORDER BY created_at"
            ),
            vec![Value::Text(id.to_string())],
        )
        .await?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        out.push(Link {
            id: text(&row, 0)?,
            reason: parse_enum(&required(&row, 1)?)?,
            source: required(&row, 2)?,
            created_at: required(&row, 3)?,
        });
    }
    Ok(out)
}

async fn link(
    conn: &turso::Connection,
    old_id: &str,
    new_id: Option<&str>,
    reason: Reason,
    source: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO supersessions (old_id, new_id, reason, source, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        vec![
            Value::Text(old_id.to_string()),
            opt_text(new_id.map(str::to_string)),
            Value::Text(enum_str(&reason)),
            Value::Text(source.to_string()),
            Value::Text(now()),
        ],
    )
    .await?;
    Ok(())
}

fn build(new: NewMemory, source: &str, origin: Origin) -> Result<Memory> {
    let name = new.name.trim().to_string();
    if name.is_empty()
        || name.len() > MAX_NAME_CHARS
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        || name.starts_with('-')
        || name.ends_with('-')
    {
        bail!(
            "name must be a lowercase slug of letters, digits, and hyphens, at most {MAX_NAME_CHARS} characters, e.g. prefers-rust: {name:?}"
        );
    }
    let description = new.description.trim().to_string();
    if description.is_empty() || description.contains('\n') {
        bail!("description must be a single non-empty line");
    }
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        bail!("description is longer than {MAX_DESCRIPTION_CHARS} characters");
    }
    if new.text.trim().is_empty() {
        bail!("body must not be empty");
    }
    if new.text.chars().count() > MAX_TEXT_CHARS {
        bail!("body is longer than {MAX_TEXT_CHARS} characters; split it into separate notes");
    }
    let scope = new.scope.trim().to_lowercase();
    if scope.is_empty() {
        bail!("scope must not be empty");
    }
    Ok(Memory {
        id: uuid::Uuid::now_v7().to_string(),
        name: Some(name),
        description: Some(description),
        text: new.text,
        kind: new
            .kind
            .map(|k| k.trim().to_lowercase())
            .filter(|k| !k.is_empty()),
        tags: normalize_tags(new.tags)?,
        scope,
        source: source.to_string(),
        origin,
        created_at: now(),
        superseded: false,
    })
}

async fn insert(conn: &turso::Connection, m: &Memory) -> Result<()> {
    conn.execute(
        "INSERT INTO memories (id, text, kind, tags, scope, source, origin, created_at, name, description)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        vec![
            Value::Text(m.id.clone()),
            Value::Text(m.text.clone()),
            opt_text(m.kind.clone()),
            opt_text((!m.tags.is_empty()).then(|| m.tags.join(","))),
            Value::Text(m.scope.clone()),
            Value::Text(m.source.clone()),
            Value::Text(enum_str(&m.origin)),
            Value::Text(m.created_at.clone()),
            opt_text(m.name.clone()),
            opt_text(m.description.clone()),
        ],
    )
    .await?;
    Ok(())
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn normalize_tags(tags: Vec<String>) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for tag in tags {
        let tag = tag.trim().to_lowercase();
        if tag.is_empty() {
            continue;
        }
        if tag.contains(',') {
            bail!("tags must not contain commas: {tag:?}");
        }
        if !out.contains(&tag) {
            out.push(tag);
        }
    }
    Ok(out)
}

fn enum_str<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        other => unreachable!("unit enum serialized as {other:?}"),
    }
}

fn parse_enum<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .with_context(|| format!("unexpected value {s:?} in database"))
}

fn opt_text(s: Option<String>) -> Value {
    s.map(Value::Text).unwrap_or(Value::Null)
}

fn text(row: &turso::Row, i: usize) -> Result<Option<String>> {
    Ok(match row.get_value(i)? {
        Value::Null => None,
        Value::Text(s) => Some(s),
        other => bail!("column {i}: expected text, got {other:?}"),
    })
}

fn required(row: &turso::Row, i: usize) -> Result<String> {
    text(row, i)?.with_context(|| format!("column {i} is unexpectedly NULL"))
}

fn row_to_memory(row: &turso::Row) -> Result<Memory> {
    Ok(Memory {
        id: required(row, 0)?,
        text: required(row, 1)?,
        kind: text(row, 2)?,
        tags: text(row, 3)?
            .map(|t| t.split(',').map(str::to_string).collect())
            .unwrap_or_default(),
        scope: required(row, 4)?,
        source: required(row, 5)?,
        origin: parse_enum(&required(row, 6)?)?,
        created_at: required(row, 7)?,
        superseded: match row.get_value(8)? {
            Value::Integer(n) => n != 0,
            other => bail!("column 8: expected integer, got {other:?}"),
        },
        name: text(row, 9)?,
        description: text(row, 10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn note(name: &str, text: &str, scope: &str, origin: Option<Origin>) -> NewMemory {
        NewMemory {
            name: name.into(),
            description: format!("about {name}"),
            text: text.into(),
            kind: Some("user".into()),
            tags: vec!["Rust".into(), " rust ".into(), "".into()],
            scope: scope.into(),
            origin,
        }
    }

    async fn temp_store() -> Result<(Store, std::path::PathBuf)> {
        let dir = std::env::temp_dir().join(format!("memory-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("t.db");
        Ok((Store::open(path.to_str().unwrap()).await?, path))
    }

    #[tokio::test]
    async fn write_read_search() -> Result<()> {
        let (store, path) = temp_store().await?;
        let a = store
            .write(
                note(
                    "prefers-rust",
                    "James prefers Rust for side projects",
                    "Personal",
                    Some(Origin::UserStated),
                ),
                None,
                "t",
            )
            .await?;
        store
            .write(
                note(
                    "withings-host",
                    "The Withings server runs on a VPS",
                    "work",
                    Some(Origin::External),
                ),
                None,
                "t",
            )
            .await?;
        assert_eq!(a.tags, vec!["rust"]);
        assert_eq!(a.scope, "personal");

        let got = store.read("prefers-rust").await?.expect("note exists");
        assert_eq!(got.memory.id, a.id);
        assert!(store.read("nope").await?.is_none());
        assert!(store.get(&a.id).await?.is_some());

        // Search covers bodies, names, and descriptions.
        assert_eq!(store.search("projects", &[], false, 10).await?[0].id, a.id);
        assert_eq!(store.search("withings", &[], false, 10).await?.len(), 1);
        let hits = store
            .search("rust OR vps", &["work".into()], false, 10)
            .await?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].scope, "work");

        drop(store);
        let store = Store::open(path.to_str().unwrap()).await?;
        assert_eq!(store.search("vps", &[], false, 10).await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn write_is_compare_and_swap() -> Result<()> {
        let (store, _) = temp_store().await?;
        let v1 = store
            .write(
                note(
                    "home",
                    "Lives in Oakland",
                    "personal",
                    Some(Origin::UserStated),
                ),
                None,
                "t",
            )
            .await?;

        // Creating again, revising the wrong version, and revising a missing note all fail.
        let again = store
            .write(
                note("home", "x", "personal", Some(Origin::UserStated)),
                None,
                "t",
            )
            .await;
        assert!(matches!(again, Err(WriteError::Conflict(_))));
        let stale = store
            .write(
                note("home", "x", "personal", Some(Origin::UserStated)),
                Some("bogus".into()),
                "t",
            )
            .await;
        assert!(matches!(stale, Err(WriteError::Conflict(_))));
        let missing = store
            .write(
                note("nope", "x", "personal", Some(Origin::UserStated)),
                Some(v1.id.clone()),
                "t",
            )
            .await;
        assert!(matches!(missing, Err(WriteError::Conflict(_))));

        let v2 = store
            .write(
                note(
                    "home",
                    "Lives near Lake Merritt in Oakland",
                    "personal",
                    Some(Origin::UserStated),
                ),
                Some(v1.id.clone()),
                "t",
            )
            .await?;
        let current = store.read("home").await?.unwrap();
        assert_eq!(current.memory.id, v2.id);
        assert_eq!(current.supersedes[0].id.as_deref(), Some(v1.id.as_str()));
        assert_eq!(current.supersedes[0].reason, Reason::Revised);
        assert!(store.get(&v1.id).await?.unwrap().memory.superseded);
        assert_eq!(store.search("lives", &[], false, 10).await?.len(), 1);

        // Forget needs the current version too.
        assert!(matches!(
            store.forget("home", &v1.id, "t").await,
            Err(WriteError::Conflict(_))
        ));
        store.forget("home", &v2.id, "t").await?;
        assert!(store.read("home").await?.is_none());
        // The name is free again.
        store
            .write(
                note(
                    "home",
                    "Lives in Berkeley",
                    "personal",
                    Some(Origin::UserStated),
                ),
                None,
                "t",
            )
            .await?;

        // Invalid names and descriptions are rejected.
        for bad in ["Home", "has space", "-lead", "a/b", ""] {
            let r = store
                .write(
                    note(bad, "x", "personal", Some(Origin::UserStated)),
                    None,
                    "t",
                )
                .await;
            assert!(matches!(r, Err(WriteError::Invalid(_))), "{bad:?} accepted");
        }
        let mut long = note("long-desc", "x", "personal", Some(Origin::UserStated));
        long.description = "x".repeat(MAX_DESCRIPTION_CHARS + 1);
        assert!(matches!(
            store.write(long, None, "t").await,
            Err(WriteError::Invalid(_))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn supersede_merge_split_retract() -> Result<()> {
        let (store, _) = temp_store().await?;
        let a = store
            .write(
                note(
                    "city",
                    "James lives in Oakland",
                    "personal",
                    Some(Origin::UserStated),
                ),
                None,
                "t",
            )
            .await?;
        let b = store
            .write(
                note(
                    "neighborhood",
                    "James lives near Lake Merritt",
                    "personal",
                    Some(Origin::External),
                ),
                None,
                "t",
            )
            .await?;
        let c = store
            .write(
                note(
                    "editor",
                    "James uses Zed",
                    "personal",
                    Some(Origin::UserStated),
                ),
                None,
                "t",
            )
            .await?;

        // A new note can't take the name of a live note that isn't being replaced.
        let clash = store
            .supersede(
                vec![a.id.clone()],
                vec![note("editor", "x", "personal", None)],
                Reason::Merged,
                "t",
            )
            .await;
        assert!(matches!(clash, Err(WriteError::Conflict(_))));

        // Merge, reusing one of the replaced names; origin defaults to the weakest input.
        let merged = store
            .supersede(
                vec![a.id.clone(), b.id.clone()],
                vec![note(
                    "city",
                    "James lives near Lake Merritt in Oakland",
                    "personal",
                    None,
                )],
                Reason::Merged,
                "consolidator",
            )
            .await?;
        let m = &merged[0];
        assert_eq!(m.origin, Origin::External);
        assert_eq!(store.read("city").await?.unwrap().memory.id, m.id);
        assert!(store.read("neighborhood").await?.is_none());

        let hits = store.search("lives", &[], false, 10).await?;
        assert_eq!(hits.iter().map(|h| &h.id).collect::<Vec<_>>(), vec![&m.id]);
        assert_eq!(store.search("lives", &[], true, 10).await?.len(), 3);

        // Compare-and-swap: superseding an already-superseded memory fails and writes nothing.
        let err = store
            .supersede(
                vec![m.id.clone(), a.id.clone()],
                vec![note("x", "x", "personal", None)],
                Reason::Corrected,
                "t",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, WriteError::AlreadySuperseded(id) if id == a.id));
        assert!(store.read("x").await?.is_none());

        // Split, with an explicit origin.
        let parts = store
            .supersede(
                vec![m.id.clone()],
                vec![
                    note(
                        "city",
                        "James lives in Oakland",
                        "personal",
                        Some(Origin::UserStated),
                    ),
                    note(
                        "neighborhood",
                        "James lives near Lake Merritt",
                        "personal",
                        None,
                    ),
                ],
                Reason::Split,
                "t",
            )
            .await?;
        assert_eq!(parts[0].origin, Origin::UserStated);
        assert_eq!(parts[1].origin, Origin::External);

        // Duplicate names in one call, and retract/replace mismatches, are rejected.
        let dup = store
            .supersede(
                vec![c.id.clone()],
                vec![note("d", "x", "p", None), note("d", "y", "p", None)],
                Reason::Split,
                "t",
            )
            .await;
        assert!(matches!(dup, Err(WriteError::Invalid(_))));
        assert!(matches!(
            store
                .supersede(vec![c.id.clone()], vec![], Reason::Corrected, "t")
                .await,
            Err(WriteError::Invalid(_))
        ));
        store
            .supersede(vec![c.id.clone()], vec![], Reason::Retracted, "t")
            .await?;
        assert_eq!(store.get(&c.id).await?.unwrap().superseded_by[0].id, None);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_revisions_only_one_wins() -> Result<()> {
        let (store, _) = temp_store().await?;
        let v1 = store
            .write(
                note("contested", "v1", "work", Some(Origin::AgentInferred)),
                None,
                "t",
            )
            .await?;
        let attempts = (0..8).map(|i| {
            let store = store.clone();
            let v1 = v1.id.clone();
            tokio::spawn(async move {
                store
                    .write(
                        note(
                            "contested",
                            &format!("rewrite {i}"),
                            "work",
                            Some(Origin::AgentInferred),
                        ),
                        Some(v1),
                        "t",
                    )
                    .await
            })
        });
        let mut wins = 0;
        for h in attempts {
            match h.await? {
                Ok(_) => wins += 1,
                Err(WriteError::Conflict(_)) => {}
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert_eq!(wins, 1);
        assert_eq!(store.get(&v1.id).await?.unwrap().superseded_by.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn index_lists_trusted_live_notes_within_budget() -> Result<()> {
        let (store, _) = temp_store().await?;
        assert_eq!(store.index(10_000), "");
        store
            .write(
                note("prefers-rust", "x", "personal", Some(Origin::UserStated)),
                None,
                "t",
            )
            .await?;
        store
            .write(
                note("deploy-target", "x", "work", Some(Origin::AgentInferred)),
                None,
                "t",
            )
            .await?;
        store
            .write(
                note("from-web", "x", "work", Some(Origin::External)),
                None,
                "t",
            )
            .await?;
        let gone = store
            .write(
                note("old-fact", "x", "work", Some(Origin::UserStated)),
                None,
                "t",
            )
            .await?;
        store.forget("old-fact", &gone.id, "t").await?;

        let index = store.index(10_000);
        assert_eq!(
            index,
            "### personal\n- prefers-rust: about prefers-rust (user)\n\n### work\n- deploy-target: about deploy-target (user)"
        );

        // Over budget, the newest notes are kept.
        store
            .write(
                note("newest", "x", "personal", Some(Origin::UserStated)),
                None,
                "t",
            )
            .await?;
        let index = store.index(150);
        assert!(index.len() <= 150, "{} chars: {index}", index.len());
        assert!(index.contains("- newest:"), "{index}");
        assert!(index.contains("2 older notes not listed"), "{index}");
        Ok(())
    }

    #[tokio::test]
    async fn migrates_pre_notes_schema() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("memory-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("old.db");
        {
            let db = Builder::new_local(path.to_str().unwrap())
                .experimental_index_method(true)
                .build()
                .await?;
            db.connect()?
                .execute_batch(
                    "CREATE TABLE memories (id TEXT PRIMARY KEY, text TEXT NOT NULL, kind TEXT, tags TEXT,
                       scope TEXT NOT NULL, source TEXT NOT NULL, origin TEXT NOT NULL, created_at TEXT NOT NULL);
                     CREATE INDEX memories_fts ON memories USING fts (text);
                     CREATE TABLE supersessions (old_id TEXT NOT NULL, new_id TEXT, reason TEXT NOT NULL,
                       source TEXT NOT NULL, created_at TEXT NOT NULL);
                     INSERT INTO memories VALUES ('legacy', 'an old fact about gardening', NULL, NULL,
                       'personal', 'claude-code', 'user_stated', '2026-09-26T00:00:00.000Z');",
                )
                .await?;
        }
        let store = Store::open(path.to_str().unwrap()).await?;
        let hits = store.search("gardening", &[], false, 10).await?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, None);
        store
            .write(
                note("new-note", "fresh", "personal", Some(Origin::UserStated)),
                None,
                "t",
            )
            .await?;
        assert_eq!(store.search("fresh", &[], false, 10).await?.len(), 1);
        // Unnamed legacy rows stay out of the index.
        assert!(!store.index(10_000).contains("legacy"));
        // Opening again is a no-op.
        drop(store);
        Store::open(path.to_str().unwrap()).await?;
        Ok(())
    }
}
