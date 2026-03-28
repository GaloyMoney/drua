use style_agent_core::embedder::Embedder;
use style_agent_core::label_store::LabelStore;
use style_agent_core::store::VectorStore;

use crate::config::Config;
use crate::indexer;

use super::replay_labels;

/// Re-index one or all repos from config.
///
/// After indexing (with auto-labeling), human labels from the JSONL file are
/// replayed on top, preserving the priority: human > ml > heuristic.
pub async fn run(config: &Config, repo_name: Option<&str>) -> anyhow::Result<()> {
    if config.repos.is_empty() {
        anyhow::bail!("No [[repos]] entries in config.");
    }

    let repos_dir = config.repos_dir();
    let embedder = Embedder::new()?;
    let db_path = config.db_path();
    let store = VectorStore::new(&db_path)?;
    store.ensure_collection()?;

    let repos_to_index = match repo_name {
        Some(name) => {
            let repo = config
                .repos
                .iter()
                .find(|r| r.name == name)
                .ok_or_else(|| anyhow::anyhow!("Repo '{name}' not found in config"))?;
            vec![repo.clone()]
        }
        None => config.repos.clone(),
    };

    let mut total_chunks = 0usize;
    for repo in &repos_to_index {
        let dest = repos_dir.join(&repo.name);
        anyhow::ensure!(
            dest.exists(),
            "Repo directory not found: {}. Run 'bootstrap' first.",
            dest.display()
        );
        println!("Reindexing {}...", repo.name);
        let chunks = indexer::index_repo(
            &dest,
            &repo.name,
            repo.paths.as_deref(),
            &repo.exclude_dirs,
            &store,
            &embedder,
        )
        .await?;
        total_chunks += chunks;
    }

    // Classify after embedding
    let mut classifier = indexer::try_load_classifier(config);
    indexer::classify_all_chunks(
        &store,
        &mut classifier,
        config.services.classifier_confidence_threshold,
    )?;

    println!(
        "\nReindexed {} repo(s), {total_chunks} total chunks",
        repos_to_index.len()
    );

    // Replay human labels onto the fresh data (human > ml > heuristic)
    let labels_path = config.labels_dir().join("labels.jsonl");
    if labels_path.exists() {
        println!("\nReplaying human labels from {}...", labels_path.display());
        let label_store = LabelStore::new(labels_path);
        let summary = replay_labels::replay(&store, &label_store)?;
        replay_labels::print_summary(&summary);
    }

    Ok(())
}
