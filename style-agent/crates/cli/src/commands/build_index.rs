use std::path::Path;

use style_agent_core::embedder::Embedder;
use style_agent_core::label_store::LabelStore;
use style_agent_core::store::VectorStore;

use crate::commands::replay_labels;
use crate::config::Config;
use crate::indexer;

/// Build the search index from pre-cloned repos on disk (for CI).
///
/// Scans `repos_dir` for subdirectories, indexes each one, then replays
/// human labels from `labels.jsonl`.
pub async fn run(config: &Config, repos_dir_arg: &str) -> anyhow::Result<()> {
    let repos_dir = Path::new(repos_dir_arg);
    anyhow::ensure!(
        repos_dir.is_dir(),
        "--repos-dir does not exist or is not a directory: {}",
        repos_dir.display()
    );

    // Discover repo subdirectories (follow symlinks, skip hidden dirs)
    let mut repo_dirs: Vec<_> = std::fs::read_dir(repos_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path().is_dir() && !e.file_name().to_string_lossy().starts_with('.')
        })
        .collect();
    repo_dirs.sort_by_key(|e| e.file_name());

    if repo_dirs.is_empty() {
        anyhow::bail!("No subdirectories found in {}", repos_dir.display());
    }

    println!(
        "Found {} repo(s) in {}",
        repo_dirs.len(),
        repos_dir.display()
    );

    // Initialize indexing resources
    let embedder = Embedder::new()?;
    let db_path = config.db_path();
    let store = VectorStore::new(&db_path)?;
    store.ensure_collection()?;

    let mut classifier = indexer::try_load_classifier(config);
    let threshold = config.services.classifier_confidence_threshold;

    // Index each repo
    let mut total_chunks = 0usize;
    for entry in &repo_dirs {
        let repo_name = entry.file_name().to_string_lossy().to_string();
        let repo_path = entry.path();

        // Look up per-repo config for exclude_dirs / paths
        let repo_config = config.repos.iter().find(|r| r.name == repo_name);

        let (subpaths, exclude_dirs): (Option<&[String]>, &[String]) = match repo_config {
            Some(rc) => (rc.paths.as_deref(), &rc.exclude_dirs),
            None => (None, &[]),
        };

        println!("Indexing {repo_name}...");
        let chunks = indexer::index_repo(
            &repo_path,
            &repo_name,
            subpaths,
            exclude_dirs,
            &store,
            &embedder,
            &mut classifier,
            threshold,
        )
        .await?;
        total_chunks += chunks;
    }

    // Replay human labels
    let labels_path = config.labels_dir().join("labels.jsonl");
    if labels_path.exists() {
        println!("\nReplaying human labels from {}...", labels_path.display());
        let label_store = LabelStore::new(labels_path);
        let summary = replay_labels::replay(&store, &label_store)?;
        replay_labels::print_summary(&summary);
    } else {
        println!("\nNo labels.jsonl found, skipping replay.");
    }

    println!("\n=== Build-index complete ===");
    println!(
        "Repos: {} | Total chunks: {total_chunks}",
        repo_dirs.len()
    );
    println!("Database: {}", db_path.display());

    Ok(())
}
