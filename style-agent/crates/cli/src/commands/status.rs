use style_agent_core::store::VectorStore;

use crate::config::Config;

/// Show service health and index stats.
pub async fn run(config: &Config) -> anyhow::Result<()> {
    println!("Embed:   fastembed (nomic-embed-text-v1.5-Q)");
    println!("Data:    {}", config.data_dir().display());

    // Show database info
    let db_path = config.db_path();
    if db_path.exists() {
        let store = VectorStore::new(&db_path)?;
        store.ensure_collection()?;
        let count = store.chunk_count()?;
        println!("\nDatabase: {}", db_path.display());
        println!("Chunks:   {count}");
    } else {
        println!(
            "\nDatabase not found at {}. Run 'bootstrap' first.",
            db_path.display()
        );
    }

    // Show configured repos
    if !config.repos.is_empty() {
        println!("\nConfigured repos:");
        let repos_dir = config.repos_dir();
        for repo in &config.repos {
            let dest = repos_dir.join(&repo.name);
            let status = if dest.exists() {
                "cloned"
            } else {
                "not cloned"
            };
            println!("  {} ({}) [{}]", repo.name, repo.url, status);
        }
    }

    Ok(())
}
