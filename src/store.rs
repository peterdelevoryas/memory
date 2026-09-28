use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use turso::{Builder, Database, Value, transaction::TransactionBehavior};

// Memory is a set of named notes, like files in a directory. Writing a note
// again replaces it in place and forgetting one deletes it; there is no
// history (nightly backups are the safety net). Each write gets a new
// `version`, which writers pass back to revise or forget a note, so two agents
// can't overwrite each other (compare-and-swap).
const TABLES: &str = "
CREATE TABLE IF NOT EXISTS notes (
  name        TEXT PRIMARY KEY,
  version     TEXT NOT NULL,
  description TEXT NOT NULL,
  text        TEXT NOT NULL,
  kind        TEXT,
  tags        TEXT,
  scope       TEXT NOT NULL,
  source      TEXT NOT NULL,
  origin      TEXT NOT NULL,
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL
);
";

const INDEXES: &str = "
CREATE INDEX IF NOT EXISTS notes_fts ON notes USING fts (name, description, text)
  WITH (weights = 'name=2.0,description=2.0,text=1.0');
";

const COLUMNS: &str = "name, description, text, kind, tags, scope, source, origin, version, \
    created_at, updated_at";

const FTS_COLUMNS: &str = "name, description, text";

pub const MAX_NAME_CHARS: usize = 64;
pub const MAX_DESCRIPTION_CHARS: usize = 150;
pub const MAX_TEXT_CHARS: usize = 4000;

/// Where a note's content came from, as declared by the agent that wrote it.
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

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Memory {
    pub name: String,
    pub description: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub tags: Vec<String>,
    pub scope: String,
    /// The client that last wrote the note.
    pub source: String,
    pub origin: Origin,
    /// Changes on every write; pass it as `expected_version` to revise or forget.
    pub version: String,
    pub created_at: String,
    pub updated_at: String,
}

pub struct NewMemory {
    pub name: String,
    pub description: String,
    pub text: String,
    pub kind: Option<String>,
    pub tags: Vec<String>,
    pub scope: String,
    pub origin: Origin,
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
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
}

impl Store {
    pub async fn open(path: &str) -> Result<Self> {
        let db = Builder::new_local(path)
            .experimental_index_method(true)
            .build()
            .await
            .with_context(|| format!("opening database {path}"))?;
        let conn = db.connect()?;
        conn.execute_batch(TABLES)
            .await
            .context("creating tables")?;
        migrate(&conn).await.context("migrating schema")?;
        conn.execute_batch(INDEXES)
            .await
            .context("creating indexes")?;
        drop(conn);
        Ok(Self { db })
    }

    fn conn(&self) -> Result<turso::Connection> {
        let conn = self.db.connect()?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(conn)
    }

    /// Creates a note, or with `expected_version` replaces one. Creating fails
    /// if the name is taken, and replacing fails unless `expected_version` is
    /// the note's current version.
    pub async fn write(
        &self,
        note: NewMemory,
        expected_version: Option<String>,
        source: &str,
    ) -> Result<Memory, WriteError> {
        let mut memory = build(note, source).map_err(|e| WriteError::Invalid(format!("{e:#}")))?;
        let mut conn = self.conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await?;
        let current = by_name(&tx, &memory.name).await?;
        let name = &memory.name;
        match (current, expected_version) {
            (None, None) => insert(&tx, &memory).await?,
            (None, Some(_)) => {
                return Err(WriteError::Conflict(format!(
                    "there is no note named {name:?} to revise (it may have been forgotten or \
                     renamed); omit expected_version to create it"
                )));
            }
            (Some(cur), None) => {
                return Err(WriteError::Conflict(format!(
                    "a note named {name:?} already exists (version {}); read it and pass \
                     expected_version to revise it, or choose another name",
                    cur.version
                )));
            }
            (Some(cur), Some(expected)) if cur.version != expected => {
                return Err(WriteError::Conflict(format!(
                    "note {name:?} changed since you read it: current version is {}, not \
                     {expected}; read it again and merge your changes",
                    cur.version
                )));
            }
            (Some(cur), Some(_)) => {
                memory.created_at = cur.created_at;
                update(&tx, &memory).await?;
            }
        }
        tx.commit().await?;
        Ok(memory)
    }

    /// Deletes the note `name` if its current version is `expected_version`.
    pub async fn forget(&self, name: &str, expected_version: &str) -> Result<(), WriteError> {
        let mut conn = self.conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await?;
        match by_name(&tx, name).await? {
            None => {
                return Err(WriteError::Conflict(format!(
                    "there is no note named {name:?}"
                )));
            }
            Some(cur) if cur.version != expected_version => {
                return Err(WriteError::Conflict(format!(
                    "note {name:?} changed since you read it: current version is {}",
                    cur.version
                )));
            }
            Some(_) => {}
        }
        tx.execute(
            "DELETE FROM notes WHERE name = ?1",
            vec![Value::Text(name.to_string())],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn read(&self, name: &str) -> Result<Option<Memory>> {
        by_name(&self.conn()?, name).await
    }

    /// Full-text search over names, descriptions, and bodies, best match
    /// first. `query` uses Tantivy query syntax; `scopes` empty means all.
    pub async fn search(&self, query: &str, scopes: &[String], limit: u32) -> Result<Vec<Memory>> {
        let mut sql = format!(
            "SELECT {COLUMNS}, fts_score({FTS_COLUMNS}, ?1) AS score FROM notes \
             WHERE fts_match({FTS_COLUMNS}, ?1)"
        );
        let mut params = vec![Value::Text(query.to_string())];
        push_scopes(&mut sql, &mut params, scopes);
        sql.push_str(&format!(" ORDER BY score DESC LIMIT {limit}"));
        let mut rows = self.conn()?.query(sql, params).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row_to_memory(&row)?);
        }
        Ok(out)
    }

    /// The index: one line per note, grouped by scope. Only notes stated by
    /// the user or inferred by an agent are listed; external content stays
    /// out (memory_list and memory_search still find it).
    pub async fn index(&self) -> Result<String> {
        let mut rows = self
            .conn()?
            .query(
                "SELECT scope, name, description, kind FROM notes \
                 WHERE origin IN ('user_stated', 'agent_inferred') ORDER BY scope, name",
                (),
            )
            .await?;
        let mut out = String::new();
        let mut current: Option<String> = None;
        while let Some(row) = rows.next().await? {
            let scope = required(&row, 0)?;
            if current.as_ref() != Some(&scope) {
                out.push_str(&format!("\n### {scope}\n"));
                current = Some(scope);
            }
            let name = required(&row, 1)?;
            let description = required(&row, 2)?;
            let kind = text(&row, 3)?
                .map(|k| format!(" ({k})"))
                .unwrap_or_default();
            out.push_str(&format!("- {name}: {description}{kind}\n"));
        }
        Ok(out.trim().to_string())
    }

    /// Every note, a page at a time, ordered by name. With `since`, only notes
    /// written or revised after it. `after` is the last name of the previous page.
    pub async fn list(
        &self,
        since: Option<&str>,
        scopes: &[String],
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<Memory>> {
        let mut sql = format!("SELECT {COLUMNS} FROM notes WHERE 1 = 1");
        let mut params = Vec::new();
        if let Some(since) = since {
            params.push(Value::Text(since.to_string()));
            sql.push_str(&format!(" AND updated_at > ?{}", params.len()));
        }
        if let Some(after) = after {
            params.push(Value::Text(after.to_string()));
            sql.push_str(&format!(" AND name > ?{}", params.len()));
        }
        push_scopes(&mut sql, &mut params, scopes);
        sql.push_str(&format!(" ORDER BY name LIMIT {limit}"));
        let mut rows = self.conn()?.query(sql, params).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row_to_memory(&row)?);
        }
        Ok(out)
    }
}

fn push_scopes(sql: &mut String, params: &mut Vec<Value>, scopes: &[String]) {
    if scopes.is_empty() {
        return;
    }
    let placeholders: Vec<String> = scopes
        .iter()
        .map(|s| {
            params.push(Value::Text(s.clone()));
            format!("?{}", params.len())
        })
        .collect();
    sql.push_str(&format!(" AND scope IN ({})", placeholders.join(", ")));
}

/// Moves databases from the versioned schema (every version of every note in
/// `memories`, plus `supersessions`) to `notes`: current notes are kept, all
/// history is dropped.
async fn migrate(conn: &turso::Connection) -> Result<()> {
    let mut rows = conn
        .query("SELECT name FROM pragma_table_info('memories')", ())
        .await?;
    let has_old = rows.next().await?.is_some();
    drop(rows);
    if !has_old {
        return Ok(());
    }
    tracing::info!("migrating to notes: keeping current notes, dropping history");
    conn.execute_batch(
        "BEGIN IMMEDIATE;
         INSERT INTO notes (name, version, description, text, kind, tags, scope, source, origin,
                            created_at, updated_at)
           SELECT m.name, m.id, m.description, m.text, m.kind, m.tags, m.scope, m.source, m.origin,
                  (SELECT MIN(o.created_at) FROM memories o WHERE o.name = m.name), m.created_at
           FROM memories m
           WHERE m.name IS NOT NULL
             AND NOT EXISTS (SELECT 1 FROM supersessions s WHERE s.old_id = m.id);
         DROP INDEX IF EXISTS memories_notes_fts;
         DROP INDEX IF EXISTS memories_fts;
         DROP INDEX IF EXISTS memories_name;
         DROP INDEX IF EXISTS supersessions_old;
         DROP INDEX IF EXISTS supersessions_new;
         DROP TABLE IF EXISTS supersessions;
         DROP TABLE memories;
         COMMIT;",
    )
    .await?;
    Ok(())
}

async fn by_name(conn: &turso::Connection, name: &str) -> Result<Option<Memory>> {
    let mut rows = conn
        .query(
            format!("SELECT {COLUMNS} FROM notes WHERE name = ?1"),
            vec![Value::Text(name.to_string())],
        )
        .await?;
    rows.next()
        .await?
        .map(|row| row_to_memory(&row))
        .transpose()
}

fn build(new: NewMemory, source: &str) -> Result<Memory> {
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
    let now = now();
    Ok(Memory {
        name,
        description,
        text: new.text,
        kind: new
            .kind
            .map(|k| k.trim().to_lowercase())
            .filter(|k| !k.is_empty()),
        tags: normalize_tags(new.tags)?,
        scope,
        source: source.to_string(),
        origin: new.origin,
        version: uuid::Uuid::now_v7().to_string(),
        created_at: now.clone(),
        updated_at: now,
    })
}

fn values(m: &Memory) -> Vec<Value> {
    vec![
        Value::Text(m.name.clone()),
        Value::Text(m.version.clone()),
        Value::Text(m.description.clone()),
        Value::Text(m.text.clone()),
        opt_text(m.kind.clone()),
        opt_text((!m.tags.is_empty()).then(|| m.tags.join(","))),
        Value::Text(m.scope.clone()),
        Value::Text(m.source.clone()),
        Value::Text(enum_str(&m.origin)),
        Value::Text(m.created_at.clone()),
        Value::Text(m.updated_at.clone()),
    ]
}

async fn insert(conn: &turso::Connection, m: &Memory) -> Result<()> {
    conn.execute(
        "INSERT INTO notes (name, version, description, text, kind, tags, scope, source, origin,
                            created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        values(m),
    )
    .await?;
    Ok(())
}

async fn update(conn: &turso::Connection, m: &Memory) -> Result<()> {
    conn.execute(
        "UPDATE notes SET version = ?2, description = ?3, text = ?4, kind = ?5, tags = ?6,
                          scope = ?7, source = ?8, origin = ?9, created_at = ?10, updated_at = ?11
         WHERE name = ?1",
        values(m),
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
        name: required(row, 0)?,
        description: required(row, 1)?,
        text: required(row, 2)?,
        kind: text(row, 3)?,
        tags: text(row, 4)?
            .map(|t| t.split(',').map(str::to_string).collect())
            .unwrap_or_default(),
        scope: required(row, 5)?,
        source: required(row, 6)?,
        origin: parse_enum(&required(row, 7)?)?,
        version: required(row, 8)?,
        created_at: required(row, 9)?,
        updated_at: required(row, 10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn note(name: &str, text: &str, scope: &str, origin: Origin) -> NewMemory {
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
                    Origin::UserStated,
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
                    Origin::External,
                ),
                None,
                "t",
            )
            .await?;
        assert_eq!(a.tags, vec!["rust"]);
        assert_eq!(a.scope, "personal");
        assert_eq!(
            store.read("prefers-rust").await?.unwrap().version,
            a.version
        );
        assert!(store.read("nope").await?.is_none());

        // Search covers bodies, names, and descriptions.
        assert_eq!(
            store.search("projects", &[], 10).await?[0].name,
            "prefers-rust"
        );
        assert_eq!(store.search("withings", &[], 10).await?.len(), 1);
        let hits = store
            .search("rust OR vps", &["work".into()], 10)
            .await?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].scope, "work");

        drop(store);
        let store = Store::open(path.to_str().unwrap()).await?;
        assert_eq!(store.search("vps", &[], 10).await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn write_replaces_in_place_and_forget_deletes() -> Result<()> {
        let (store, _) = temp_store().await?;
        let v1 = store
            .write(
                note("home", "Lives in Oakland", "personal", Origin::UserStated),
                None,
                "a",
            )
            .await?;

        // Creating again, revising the wrong version, and revising a missing note all fail.
        let again = store
            .write(note("home", "x", "personal", Origin::UserStated), None, "a")
            .await;
        assert!(matches!(again, Err(WriteError::Conflict(_))));
        let stale = store
            .write(
                note("home", "x", "personal", Origin::UserStated),
                Some("bogus".into()),
                "a",
            )
            .await;
        assert!(matches!(stale, Err(WriteError::Conflict(_))));
        let missing = store
            .write(
                note("nope", "x", "personal", Origin::UserStated),
                Some(v1.version.clone()),
                "a",
            )
            .await;
        assert!(matches!(missing, Err(WriteError::Conflict(_))));

        let v2 = store
            .write(
                note(
                    "home",
                    "Lives near Lake Merritt in Oakland",
                    "personal",
                    Origin::AgentInferred,
                ),
                Some(v1.version.clone()),
                "b",
            )
            .await?;
        assert_ne!(v2.version, v1.version);
        let current = store.read("home").await?.unwrap();
        assert_eq!(current.text, "Lives near Lake Merritt in Oakland");
        assert_eq!(current.source, "b");
        assert_eq!(current.created_at, v1.created_at);
        assert!(current.updated_at >= v1.updated_at);

        // The replaced text is gone from search; the new text is found.
        assert!(store.search("merritt", &[], 10).await?.len() == 1);
        assert_eq!(store.search("lives", &[], 10).await?.len(), 1);

        // Forget needs the current version, then really deletes.
        assert!(matches!(
            store.forget("home", &v1.version).await,
            Err(WriteError::Conflict(_))
        ));
        store.forget("home", &v2.version).await?;
        assert!(store.read("home").await?.is_none());
        assert!(
            store
                .search("oakland OR merritt", &[], 10)
                .await?
                .is_empty()
        );
        assert!(matches!(
            store.forget("home", &v2.version).await,
            Err(WriteError::Conflict(_))
        ));
        // The name is free again.
        store
            .write(
                note("home", "Lives in Berkeley", "personal", Origin::UserStated),
                None,
                "a",
            )
            .await?;

        // Invalid names and descriptions are rejected.
        for bad in ["Home", "has space", "-lead", "a/b", ""] {
            let r = store
                .write(note(bad, "x", "personal", Origin::UserStated), None, "a")
                .await;
            assert!(matches!(r, Err(WriteError::Invalid(_))), "{bad:?} accepted");
        }
        let mut long = note("long-desc", "x", "personal", Origin::UserStated);
        long.description = "x".repeat(MAX_DESCRIPTION_CHARS + 1);
        assert!(matches!(
            store.write(long, None, "a").await,
            Err(WriteError::Invalid(_))
        ));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_revisions_only_one_wins() -> Result<()> {
        let (store, _) = temp_store().await?;
        let v1 = store
            .write(
                note("contested", "v1", "work", Origin::AgentInferred),
                None,
                "t",
            )
            .await?;
        let attempts = (0..8).map(|i| {
            let store = store.clone();
            let v1 = v1.version.clone();
            tokio::spawn(async move {
                store
                    .write(
                        note(
                            "contested",
                            &format!("rewrite {i}"),
                            "work",
                            Origin::AgentInferred,
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
        Ok(())
    }

    #[tokio::test]
    async fn index_lists_every_trusted_note() -> Result<()> {
        let (store, _) = temp_store().await?;
        assert_eq!(store.index().await?, "");
        for (name, scope, origin) in [
            ("prefers-rust", "personal", Origin::UserStated),
            ("deploy-target", "work", Origin::AgentInferred),
            ("from-web", "work", Origin::External),
        ] {
            store
                .write(note(name, "x", scope, origin), None, "t")
                .await?;
        }
        assert_eq!(
            store.index().await?,
            "### personal\n- prefers-rust: about prefers-rust (user)\n\n### work\n- deploy-target: about deploy-target (user)"
        );
        for i in 0..100 {
            store
                .write(
                    note(&format!("n-{i:03}"), "x", "agent", Origin::UserStated),
                    None,
                    "t",
                )
                .await?;
        }
        let index = store.index().await?;
        assert!(index.contains("- n-000:") && index.contains("- n-099:"));
        Ok(())
    }

    #[tokio::test]
    async fn list_pages_and_changes_since() -> Result<()> {
        let (store, _) = temp_store().await?;
        let mut versions = Vec::new();
        for i in 0..5 {
            let scope = if i % 2 == 0 { "personal" } else { "work" };
            let m = store
                .write(
                    note(&format!("n-{i}"), "x", scope, Origin::External),
                    None,
                    "t",
                )
                .await?;
            versions.push(m.version);
        }
        let page1 = store.list(None, &[], None, 2).await?;
        assert_eq!(
            page1.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["n-0", "n-1"]
        );
        let page2 = store.list(None, &[], Some("n-1"), 2).await?;
        assert_eq!(page2[0].name, "n-2");
        assert_eq!(store.list(None, &[], Some("n-3"), 10).await?.len(), 1);
        assert_eq!(store.list(None, &["work".into()], None, 10).await?.len(), 2);

        let checkpoint = now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        store
            .write(
                note("n-0", "revised", "personal", Origin::UserStated),
                Some(versions[0].clone()),
                "t",
            )
            .await?;
        store
            .write(
                note("n-9", "new", "personal", Origin::UserStated),
                None,
                "t",
            )
            .await?;
        store.forget("n-1", &versions[1]).await?;
        let changed = store.list(Some(&checkpoint), &[], None, 10).await?;
        assert_eq!(
            changed.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["n-0", "n-9"]
        );
        Ok(())
    }

    #[tokio::test]
    async fn migrates_versioned_schema() -> Result<()> {
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
                       scope TEXT NOT NULL, source TEXT NOT NULL, origin TEXT NOT NULL,
                       created_at TEXT NOT NULL, name TEXT, description TEXT);
                     CREATE INDEX memories_notes_fts ON memories USING fts (name, description, text);
                     CREATE TABLE supersessions (old_id TEXT NOT NULL, new_id TEXT, reason TEXT NOT NULL,
                       source TEXT NOT NULL, created_at TEXT NOT NULL);
                     INSERT INTO memories VALUES
                       ('v1', 'secret old text', 'user', 'a,b', 'health', 'x', 'user_stated', '2026-09-01T00:00:00.000Z', 'skin', 'd1'),
                       ('v2', 'current text about eczema', 'user', 'a,b', 'health', 'y', 'user_stated', '2026-09-02T00:00:00.000Z', 'skin', 'd2'),
                       ('gone', 'forgotten text', NULL, NULL, 'work', 'x', 'agent_inferred', '2026-09-01T00:00:00.000Z', 'old-note', 'd'),
                       ('legacy', 'unnamed row', NULL, NULL, 'work', 'x', 'user_stated', '2026-08-01T00:00:00.000Z', NULL, NULL);
                     INSERT INTO supersessions VALUES
                       ('v1', 'v2', 'revised', 'y', '2026-09-02T00:00:00.000Z'),
                       ('gone', NULL, 'retracted', 'x', '2026-09-03T00:00:00.000Z'),
                       ('legacy', NULL, 'retracted', 'x', '2026-09-03T00:00:00.000Z');",
                )
                .await?;
        }
        let store = Store::open(path.to_str().unwrap()).await?;
        let all = store.list(None, &[], None, 10).await?;
        assert_eq!(all.len(), 1);
        let skin = &all[0];
        assert_eq!((skin.name.as_str(), skin.version.as_str()), ("skin", "v2"));
        assert_eq!(skin.text, "current text about eczema");
        assert_eq!(skin.created_at, "2026-09-01T00:00:00.000Z");
        assert_eq!(skin.updated_at, "2026-09-02T00:00:00.000Z");
        assert_eq!(skin.tags, vec!["a", "b"]);
        // Old versions and forgotten notes are really gone, including from search.
        assert_eq!(store.search("eczema", &[], 10).await?.len(), 1);
        assert!(
            store
                .search("secret OR forgotten OR unnamed", &[], 10)
                .await?
                .is_empty()
        );
        // The migrated note can be revised, and opening again is a no-op.
        store
            .write(
                note("skin", "revised", "health", Origin::UserStated),
                Some("v2".into()),
                "t",
            )
            .await?;
        drop(store);
        let store = Store::open(path.to_str().unwrap()).await?;
        assert_eq!(store.read("skin").await?.unwrap().text, "revised");
        Ok(())
    }
}
