use std::sync::Arc;

use style_agent_core::embedder::Embedder;
use style_agent_core::search::SearchEngine;
use style_agent_core::store::VectorStore;

use crate::config::Config;

/// Start the HTTP MCP server.
pub async fn run(config: &Config) -> anyhow::Result<()> {
    let db_path = config.db_path();
    tracing::info!(db = %db_path.display(), "Starting services");

    let embedder = Embedder::new()?;
    let store = VectorStore::new(&db_path)?;
    store.ensure_collection()?;
    store.ensure_anti_pattern_tables()?;
    let search_engine = Arc::new(SearchEngine::new(embedder, store));

    let bind_addr = config.bind_addr();
    style_agent_server::run_server(search_engine, &bind_addr).await
}
