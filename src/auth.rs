//! Bearer-token auth. Each client gets its own token; the tokens file stores
//! only SHA-256 hashes, one client per line: `<sha256-hex> <source> <level>`.

use std::{collections::HashMap, path::Path, sync::Arc};

use anyhow::{Context, Result, bail};
use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Read,
    Add,
    Consolidate,
}

impl std::str::FromStr for Level {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "read" => Level::Read,
            "add" => Level::Add,
            "consolidate" => Level::Consolidate,
            _ => bail!("unknown level {s:?} (expected read, add, or consolidate)"),
        })
    }
}

/// The authenticated caller, attached to each request's extensions.
#[derive(Debug, Clone)]
pub struct Client {
    pub source: String,
    pub level: Level,
}

#[derive(Clone)]
pub struct Tokens(Arc<HashMap<String, Client>>);

impl Tokens {
    pub fn load(path: &Path) -> Result<Self> {
        let contents =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut map = HashMap::new();
        for (n, line) in contents.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [hash, source, level] = fields[..] else {
                bail!("{}:{}: expected `<sha256> <source> <level>`", path.display(), n + 1);
            };
            let client = Client { source: source.to_string(), level: level.parse()? };
            if map.insert(hash.to_lowercase(), client).is_some() {
                bail!("{}:{}: duplicate token hash", path.display(), n + 1);
            }
        }
        Ok(Self(Arc::new(map)))
    }

    fn lookup(&self, token: &str) -> Option<&Client> {
        self.0.get(&hash(token))
    }
}

pub fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn generate() -> String {
    let bytes: [u8; 32] = rand::random();
    format!("mem_{}", hex::encode(bytes))
}

pub async fn middleware(State(tokens): State<Tokens>, mut req: Request, next: Next) -> Response {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match token.and_then(|t| tokens.lookup(t)) {
        Some(client) => {
            req.extensions_mut().insert(client.clone());
            next.run(req).await
        }
        None => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "missing or invalid bearer token\n",
        )
            .into_response(),
    }
}
