use std::path::Path;

use style_agent_core::embedder::Embedder;
use style_agent_core::store::VectorStore;

use crate::config::Config;
use crate::indexer;

/// Index a local directory by path (manual / ad-hoc indexing).
pub async fn run(
    config: &Config,
    repo_path: &str,
    name: &str,
    db_override: Option<&str>,
) -> anyhow::Result<()> {
    let db_path = match db_override {
        Some(name) => config.data_dir().join(format!("{name}.db")),
        None => config.db_path(),
    };

    let embedder = Embedder::new()?;
    let store = VectorStore::new(&db_path)?;
    store.ensure_collection()?;

    let path = Path::new(repo_path);
    println!("Indexing {name} from {}...", path.display());
    let chunks = indexer::index_repo(path, name, None, &[], &store, &embedder).await?;

    // Classify after embedding
    let mut classifier = indexer::try_load_classifier(config);
    indexer::classify_all_chunks(
        &store,
        &mut classifier,
        config.services.classifier_confidence_threshold,
    )?;

    println!("Done: {chunks} chunks indexed for {name}");
    Ok(())
}
