use std::path::Path;

use style_agent_core::embedder::Embedder;
use style_agent_core::store::VectorStore;

use crate::config::Config;
use crate::indexer;

/// Clone repos from config and index everything.
pub async fn run(config: &Config) -> anyhow::Result<()> {
    if config.repos.is_empty() {
        anyhow::bail!("No [[repos]] entries in config. Nothing to bootstrap.");
    }

    // 1. Create data/repos directory
    let repos_dir = config.repos_dir();
    std::fs::create_dir_all(&repos_dir)?;
    println!("Repos directory: {}", repos_dir.display());

    // 2. Clone or update repos
    println!("\n=== Cloning / updating repos ===");
    for repo in &config.repos {
        let dest = repos_dir.join(&repo.name);
        if dest.exists() {
            println!("  {}: pulling latest...", repo.name);
            git_pull(&dest, &repo.branch)?;
        } else {
            println!("  {}: cloning {}...", repo.name, repo.url);
            git_clone(&repo.url, &dest, &repo.branch)?;
        }
    }

    // 3. Index all repos (embed + store)
    println!("\n=== Indexing repos ===");
    let embedder = Embedder::new()?;
    let db_path = config.db_path();
    let store = VectorStore::new(&db_path)?;
    store.ensure_collection()?;

    let mut total_chunks = 0usize;
    for repo in &config.repos {
        let dest = repos_dir.join(&repo.name);
        println!("Indexing {}...", repo.name);
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

    // 4. Classify all chunks (ML + heuristic fallback)
    let mut classifier = indexer::try_load_classifier(config);
    indexer::classify_all_chunks(
        &store,
        &mut classifier,
        config.services.classifier_confidence_threshold,
    )?;

    println!("\n=== Bootstrap complete ===");
    println!(
        "Repos: {} | Total chunks: {total_chunks}",
        config.repos.len()
    );

    Ok(())
}

fn git_clone(url: &str, dest: &Path, branch: &str) -> anyhow::Result<()> {
    let status = std::process::Command::new("git")
        .args(["clone", "--branch", branch, "--depth", "1", url])
        .arg(dest)
        .status()?;
    anyhow::ensure!(status.success(), "git clone failed for {url}");
    Ok(())
}

fn git_pull(repo: &Path, branch: &str) -> anyhow::Result<()> {
    let status = std::process::Command::new("git")
        .args(["fetch", "origin", branch])
        .current_dir(repo)
        .status()?;
    if !status.success() {
        tracing::warn!(path = %repo.display(), "git fetch failed, skipping");
        return Ok(());
    }
    let status = std::process::Command::new("git")
        .args(["reset", "--hard", &format!("origin/{branch}")])
        .current_dir(repo)
        .status()?;
    anyhow::ensure!(status.success(), "git reset failed for {}", repo.display());
    Ok(())
}
