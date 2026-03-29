use code_assistant_core::embedder::Embedder;
use code_assistant_core::review_miner::{get_github_token, MiningStats, ReviewMiner};

use crate::config::Config;

/// Mine PR review comments from configured GitHub repos and store anti-patterns.
pub async fn run(config: &Config) -> anyhow::Result<()> {
    let mining = &config.mining;

    // Collect repos that have a github identifier
    let mineable_repos: Vec<_> = config
        .repos
        .iter()
        .filter_map(|r| r.github.as_ref().map(|gh| (r.name.clone(), gh.clone())))
        .collect();

    if mineable_repos.is_empty() {
        anyhow::bail!(
            "No repos with 'github' field in config. Add github = \"org/repo\" to [[repos]] entries."
        );
    }

    if mining.reviewers.is_empty() {
        anyhow::bail!("No reviewers configured in [mining].reviewers.");
    }

    // Get GitHub token
    println!("Authenticating with GitHub...");
    let token = get_github_token()?;
    println!("  Authenticated");

    // Set up miner
    let embedder = Embedder::new()?;
    let db_path = config.db_path();
    let miner = ReviewMiner::new(&token, &db_path, embedder)?;
    miner.ensure_collection()?;

    println!(
        "\nMining review comments from {} repo(s)",
        mineable_repos.len()
    );
    println!("Target reviewers: {}", mining.reviewers.join(", "));

    let mut totals = MiningStats::default();

    for (name, github_repo) in &mineable_repos {
        println!("\n=== {name} ({github_repo}) ===");

        match miner.mine_repo(github_repo, &mining.reviewers).await {
            Ok(stats) => {
                totals.total_fetched += stats.total_fetched;
                totals.filtered += stats.filtered;
                totals.with_fix += stats.with_fix;
                totals.stored += stats.stored;
            }
            Err(e) => {
                println!("  Error mining {github_repo}: {e}");
                tracing::error!(repo = github_repo, error = %e, "Failed to mine repo");
            }
        }
    }

    // Summary
    let existing = miner.collection_count().unwrap_or(0);
    println!("\n=== Mining Summary ===");
    println!("Repos mined:       {}", mineable_repos.len());
    println!("Comments fetched:  {}", totals.total_fetched);
    println!("Reviewer comments: {}", totals.filtered);
    println!("With fixes:        {}", totals.with_fix);
    println!("Stored:            {}", totals.stored);
    println!("Collection total:  {existing}");

    Ok(())
}
