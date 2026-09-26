mod auth;
mod server;
mod store;

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result, bail};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};

const USAGE: &str = "\
usage:
  memory serve                 run the MCP server
  memory token <source> <level>  mint a client token (level: read, add, consolidate)

environment:
  MEMORY_DB             database path (default: memory.db)
  MEMORY_TOKENS         tokens file (default: tokens)
  MEMORY_ADDR           listen address (default: 127.0.0.1:8750)
  MEMORY_ALLOWED_HOSTS  comma-separated Host headers to accept (default: localhost)";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "memory=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["serve"] => serve().await,
        ["token", source, level] => {
            let _: auth::Level = level.parse()?;
            if source.is_empty() || source.contains(char::is_whitespace) {
                bail!("source must be a single word, e.g. claude-code");
            }
            let token = auth::generate();
            eprintln!("Token (give this to the client; it is not stored):\n{token}\n");
            eprintln!("Append this line to the tokens file:");
            println!("{} {source} {level}", auth::hash(&token));
            Ok(())
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

async fn serve() -> Result<()> {
    let env = |k: &str, default: &str| std::env::var(k).unwrap_or_else(|_| default.to_string());
    let db_path = env("MEMORY_DB", "memory.db");
    let tokens_path = PathBuf::from(env("MEMORY_TOKENS", "tokens"));
    let addr = env("MEMORY_ADDR", "127.0.0.1:8750");
    let allowed_hosts: Vec<String> = env("MEMORY_ALLOWED_HOSTS", "localhost,127.0.0.1,::1")
        .split(',')
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .collect();

    let store = store::Store::open(&db_path).await?;
    let tokens = auth::Tokens::load(&tokens_path)?;

    let mcp = StreamableHttpService::new(
        move || Ok(server::MemoryServer::new(store.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().with_allowed_hosts(allowed_hosts),
    );
    let app = axum::Router::new()
        .nest_service("/mcp", mcp)
        .layer(axum::middleware::from_fn_with_state(tokens, auth::middleware));

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!("listening on http://{addr}/mcp (db: {db_path})");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
