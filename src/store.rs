use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use turso::{Builder, Database, Value, transaction::TransactionBehavior};

// Memories are never edited or deleted. Replacing one records a row in
// `supersessions`; a memory is live iff it appears in no row's `old_id`.
// A retraction is a supersession with no `new_id`.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memories (
  id          TEXT PRIMARY KEY,
  text        TEXT NOT NULL,
  kind        TEXT,
  tags        TEXT,
  scope       TEXT NOT NULL,
  source      TEXT NOT NULL,
  origin      TEXT NOT NULL,
  created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS memories_fts ON memories USING fts (text);

CREATE TABLE IF NOT EXISTS supersessions (
  old_id      TEXT NOT NULL REFERENCES memories(id),
  new_id      TEXT REFERENCES memories(id),
  reason      TEXT NOT NULL,
  source      TEXT NOT NULL,
  created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS supersessions_old ON supersessions (old_id);
CREATE INDEX IF NOT EXISTS supersessions_new ON supersessions (new_id);
";

const COLUMNS: &str = "memories.id, memories.text, memories.kind, memories.tags, memories.scope, \
    memories.source, memories.origin, memories.created_at, \
    EXISTS (SELECT 1 FROM supersessions WHERE supersessions.old_id = memories.id)";

const LIVE: &str =
    "NOT EXISTS (SELECT 1 FROM supersessions WHERE supersessions.old_id = memories.id)";

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
    /// Memories this one replaced.
    pub supersedes: Vec<Link>,
    /// What replaced this memory, if anything.
    pub superseded_by: Vec<Link>,
}

pub struct NewMemory {
    pub text: String,
    pub kind: Option<String>,
    pub tags: Vec<String>,
    pub scope: String,
    /// For a supersession, `None` means the least trusted origin of the old memories.
    pub origin: Option<Origin>,
}

#[derive(Debug, thiserror::Error)]
pub enum SupersedeError {
    #[error("no memory with id {0}")]
    NotFound(String),
    #[error("memory {0} has already been superseded; fetch it to see what replaced it")]
    AlreadySuperseded(String),
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<turso::Error> for SupersedeError {
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
        let store = Self { db };
        store
            .conn()?
            .execute_batch(SCHEMA)
            .await
            .context("applying schema")?;
        Ok(store)
    }

    fn conn(&self) -> Result<turso::Connection> {
        let conn = self.db.connect()?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(conn)
    }

    pub async fn add(&self, new: NewMemory, source: &str) -> Result<Memory> {
        let origin = new.origin.context("origin is required")?;
        let memory = build(new, source, origin)?;
        insert(&self.conn()?, &memory).await?;
        Ok(memory)
    }

    /// Atomically replace `old_ids` with `new` memories. Compare-and-swap:
    /// fails without writing anything if any old memory is missing or already
    /// superseded.
    pub async fn supersede(
        &self,
        old_ids: Vec<String>,
        new: Vec<NewMemory>,
        reason: Reason,
        source: &str,
    ) -> Result<Vec<Memory>, SupersedeError> {
        let mut old_ids = old_ids;
        old_ids.sort();
        old_ids.dedup();
        if old_ids.is_empty() {
            return Err(SupersedeError::Invalid("old_ids must not be empty".into()));
        }
        match (reason, new.is_empty()) {
            (Reason::Retracted, false) => {
                return Err(SupersedeError::Invalid(
                    "a retraction takes no new memories".into(),
                ));
            }
            (r, true) if r != Reason::Retracted => {
                return Err(SupersedeError::Invalid(
                    "new must not be empty; use reason \"retracted\" to forget without replacing"
                        .into(),
                ));
            }
            _ => {}
        }

        let mut conn = self.conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await?;

        let mut weakest = Origin::UserStated;
        for id in &old_ids {
            let mut rows = tx
                .query(
                    format!("SELECT {COLUMNS} FROM memories WHERE memories.id = ?1"),
                    vec![Value::Text(id.clone())],
                )
                .await?;
            let Some(row) = rows.next().await? else {
                return Err(SupersedeError::NotFound(id.clone()));
            };
            let old = row_to_memory(&row)?;
            if old.superseded {
                return Err(SupersedeError::AlreadySuperseded(id.clone()));
            }
            weakest = weakest.min(old.origin);
        }

        let now = now();
        let mut created = Vec::new();
        for n in new {
            let origin = n.origin.unwrap_or(weakest);
            let memory =
                build(n, source, origin).map_err(|e| SupersedeError::Invalid(format!("{e:#}")))?;
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
                tx.execute(
                    "INSERT INTO supersessions (old_id, new_id, reason, source, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    vec![
                        Value::Text(old_id.clone()),
                        opt_text(new_id.map(str::to_string)),
                        Value::Text(enum_str(&reason)),
                        Value::Text(source.to_string()),
                        Value::Text(now.clone()),
                    ],
                )
                .await?;
            }
        }
        tx.commit().await?;
        Ok(created)
    }

    pub async fn get(&self, id: &str) -> Result<Option<MemoryDetail>> {
        let conn = self.conn()?;
        let mut rows = conn
            .query(
                format!("SELECT {COLUMNS} FROM memories WHERE memories.id = ?1"),
                vec![Value::Text(id.to_string())],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        let memory = row_to_memory(&row)?;
        let supersedes = links(&conn, "new_id", "old_id", id).await?;
        let superseded_by = links(&conn, "old_id", "new_id", id).await?;
        Ok(Some(MemoryDetail {
            memory,
            supersedes,
            superseded_by,
        }))
    }

    /// Full-text search, best match first. `query` uses Tantivy query syntax;
    /// `scopes` empty means all scopes.
    pub async fn search(
        &self,
        query: &str,
        scopes: &[String],
        include_superseded: bool,
        limit: u32,
    ) -> Result<Vec<Memory>> {
        let mut sql = format!(
            "SELECT {COLUMNS}, fts_score(text, ?1) AS score FROM memories WHERE fts_match(text, ?1)"
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

fn build(new: NewMemory, source: &str, origin: Origin) -> Result<Memory> {
    if new.text.trim().is_empty() {
        bail!("text must not be empty");
    }
    let scope = new.scope.trim().to_lowercase();
    if scope.is_empty() {
        bail!("scope must not be empty");
    }
    Ok(Memory {
        id: uuid::Uuid::now_v7().to_string(),
        text: new.text,
        kind: new.kind.filter(|k| !k.trim().is_empty()),
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
        "INSERT INTO memories (id, text, kind, tags, scope, source, origin, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        vec![
            Value::Text(m.id.clone()),
            Value::Text(m.text.clone()),
            opt_text(m.kind.clone()),
            opt_text((!m.tags.is_empty()).then(|| m.tags.join(","))),
            Value::Text(m.scope.clone()),
            Value::Text(m.source.clone()),
            Value::Text(enum_str(&m.origin)),
            Value::Text(m.created_at.clone()),
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(text: &str, scope: &str, origin: Option<Origin>) -> NewMemory {
        NewMemory {
            text: text.into(),
            kind: Some("fact".into()),
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
    async fn add_get_search() -> Result<()> {
        let (store, path) = temp_store().await?;
        let a = store
            .add(
                new(
                    "James prefers Rust for side projects",
                    "Personal",
                    Some(Origin::UserStated),
                ),
                "t",
            )
            .await?;
        store
            .add(
                new(
                    "The Withings server runs on a VPS",
                    "work",
                    Some(Origin::External),
                ),
                "t",
            )
            .await?;
        assert_eq!(a.tags, vec!["rust"]);
        assert_eq!(a.scope, "personal");

        let got = store.get(&a.id).await?.expect("memory exists");
        assert_eq!(got.memory.text, a.text);
        assert_eq!(got.memory.origin, Origin::UserStated);
        assert!(store.get("nope").await?.is_none());
        assert!(store.add(new("x", "work", None), "t").await.is_err());

        let hits = store.search("rust", &[], false, 10).await?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, a.id);

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
    async fn supersede_merge_split_retract() -> Result<()> {
        let (store, _) = temp_store().await?;
        let a = store
            .add(
                new(
                    "James lives in Oakland",
                    "personal",
                    Some(Origin::UserStated),
                ),
                "t",
            )
            .await?;
        let b = store
            .add(
                new(
                    "James lives near Lake Merritt",
                    "personal",
                    Some(Origin::External),
                ),
                "t",
            )
            .await?;

        // Merge: origin defaults to the weakest input.
        let merged = store
            .supersede(
                vec![a.id.clone(), b.id.clone()],
                vec![new(
                    "James lives near Lake Merritt in Oakland",
                    "personal",
                    None,
                )],
                Reason::Merged,
                "consolidator",
            )
            .await?;
        assert_eq!(merged.len(), 1);
        let m = &merged[0];
        assert_eq!(m.origin, Origin::External);
        assert_eq!(m.source, "consolidator");

        // Search sees only the live memory unless asked.
        let hits = store.search("lives", &[], false, 10).await?;
        assert_eq!(hits.iter().map(|h| &h.id).collect::<Vec<_>>(), vec![&m.id]);
        let all = store.search("lives", &[], true, 10).await?;
        assert_eq!(all.len(), 3);
        assert_eq!(all.iter().filter(|h| h.superseded).count(), 2);

        // Links in both directions.
        let detail = store.get(&m.id).await?.unwrap();
        assert_eq!(detail.supersedes.len(), 2);
        assert!(detail.superseded_by.is_empty());
        let old = store.get(&a.id).await?.unwrap();
        assert!(old.memory.superseded);
        assert_eq!(old.superseded_by[0].id.as_deref(), Some(m.id.as_str()));
        assert_eq!(old.superseded_by[0].reason, Reason::Merged);

        // Compare-and-swap: superseding an already-superseded memory fails and writes nothing.
        let err = store
            .supersede(
                vec![m.id.clone(), a.id.clone()],
                vec![new("x", "personal", None)],
                Reason::Corrected,
                "t",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SupersedeError::AlreadySuperseded(id) if id == a.id));
        assert!(!store.get(&m.id).await?.unwrap().memory.superseded);
        assert!(store.search("x", &[], true, 10).await?.is_empty());

        let err = store
            .supersede(vec!["nope".into()], vec![], Reason::Retracted, "t")
            .await
            .unwrap_err();
        assert!(matches!(err, SupersedeError::NotFound(_)));

        // Split, with an explicit origin.
        let parts = store
            .supersede(
                vec![m.id.clone()],
                vec![
                    new(
                        "James lives in Oakland",
                        "personal",
                        Some(Origin::UserStated),
                    ),
                    new("James lives near Lake Merritt", "personal", None),
                ],
                Reason::Split,
                "t",
            )
            .await?;
        assert_eq!(parts[0].origin, Origin::UserStated);
        assert_eq!(parts[1].origin, Origin::External);
        assert_eq!(store.get(&m.id).await?.unwrap().superseded_by.len(), 2);

        // Retract: no replacement.
        assert!(
            store
                .supersede(vec![parts[1].id.clone()], vec![], Reason::Retracted, "t")
                .await?
                .is_empty()
        );
        let r = store.get(&parts[1].id).await?.unwrap();
        assert_eq!(r.superseded_by[0].id, None);
        assert_eq!(store.search("merritt", &[], false, 10).await?.len(), 0);

        // Retract with replacements, or replace with none, is rejected.
        assert!(matches!(
            store
                .supersede(vec![parts[0].id.clone()], vec![], Reason::Corrected, "t")
                .await,
            Err(SupersedeError::Invalid(_))
        ));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_supersede_only_one_wins() -> Result<()> {
        let (store, _) = temp_store().await?;
        let a = store
            .add(
                new("contested fact", "work", Some(Origin::AgentInferred)),
                "t",
            )
            .await?;
        let attempts = (0..8).map(|i| {
            let store = store.clone();
            let id = a.id.clone();
            tokio::spawn(async move {
                store
                    .supersede(
                        vec![id],
                        vec![new(&format!("rewrite {i}"), "work", None)],
                        Reason::Corrected,
                        "t",
                    )
                    .await
            })
        });
        let mut wins = 0;
        for h in attempts {
            match h.await? {
                Ok(_) => wins += 1,
                Err(SupersedeError::AlreadySuperseded(_)) => {}
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert_eq!(wins, 1);
        assert_eq!(store.get(&a.id).await?.unwrap().superseded_by.len(), 1);
        Ok(())
    }
}
