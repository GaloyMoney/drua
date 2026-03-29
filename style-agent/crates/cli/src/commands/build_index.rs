use std::path::Path;

use style_agent_core::embedder::Embedder;
use style_agent_core::label_store::LabelStore;
use style_agent_core::store::VectorStore;

use crate::commands::replay_labels;
use crate::config::Config;
use crate::indexer;

/// Build the search index from pre-cloned repos on disk (for CI).
///
/// Uses a two-phase pipeline so the embedder and classifier ONNX models
/// are never loaded at the same time, keeping peak memory low enough for
/// memory-constrained CI workers.
///
/// - Phase 1 — embed all repos (only embedder loaded), then drop it.
/// - Phase 2 — classify all stored chunks (only classifier loaded).
/// - Phase 3 — replay human labels from `labels.jsonl`.
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
        .filter(|e| e.path().is_dir() && !e.file_name().to_string_lossy().starts_with('.'))
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

    let db_path = config.db_path();
    let store = VectorStore::new(&db_path)?;
    store.ensure_collection()?;

    // ── Phase 1: embed + store (only embedder ONNX model in memory) ──
    println!("\n=== Phase 1: Embedding ===");
    let mut total_chunks = 0usize;
    {
        let embedder = Embedder::new()?;
        let mut no_classifier = None;

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
                &mut no_classifier,
                config.services.classifier_confidence_threshold,
            )
            .await?;
            total_chunks += chunks;
        }
        // `embedder` (and its ONNX session) is dropped here
    }

    // ── Phase 2: classify from DB (only classifier ONNX model in memory) ──
    println!("\n=== Phase 2: Classification ===");
    {
        let mut classifier = indexer::try_load_classifier(config);
        let threshold = config.services.classifier_confidence_threshold;
        indexer::classify_all_chunks(&store, &mut classifier, threshold)?;
        // `classifier` (and its ONNX session) is dropped here
    }

    // ── Phase 3: replay human labels ──
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
    println!("Repos: {} | Total chunks: {total_chunks}", repo_dirs.len());
    println!("Database: {}", db_path.display());

    Ok(())
}
