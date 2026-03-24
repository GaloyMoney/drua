use style_agent_core::label_store::{content_hash, LabelStore};
use style_agent_core::store::VectorStore;

use crate::config::Config;

/// Replay summary printed after label application.
pub struct ReplaySummary {
    pub exact: usize,
    pub stale: usize,
    pub not_found: usize,
}

/// Run the replay logic: load JSONL labels and apply them to stored chunks.
///
/// Returns a summary of how many labels were applied.
pub fn replay(store: &VectorStore, label_store: &LabelStore) -> anyhow::Result<ReplaySummary> {
    let records = label_store.load_all()?;

    if records.is_empty() {
        return Ok(ReplaySummary {
            exact: 0,
            stale: 0,
            not_found: 0,
        });
    }

    let chunks = store.scroll_all_chunks()?;

    let mut exact = 0usize;
    let mut stale = 0usize;
    let mut not_found = 0usize;

    for chunk in &chunks {
        let key = chunk.chunk_key();

        match records.get(&key) {
            Some(record) => {
                let current_hash = content_hash(&chunk.content);

                if current_hash == record.content_hash {
                    // Exact match — apply human labels as-is
                    store.set_chunk_labels(&chunk.id, &record.labels, "human", true)?;
                    exact += 1;
                } else {
                    // Content changed — apply labels but flag as stale
                    store.set_chunk_labels(&chunk.id, &record.labels, "human_stale", false)?;
                    stale += 1;
                }
            }
            None => {
                not_found += 1;
            }
        }
    }

    Ok(ReplaySummary {
        exact,
        stale,
        not_found,
    })
}

/// Print a replay summary to stdout.
pub fn print_summary(summary: &ReplaySummary) {
    println!("\nLabel replay summary:");
    println!("  {} exact matches applied", summary.exact);
    println!(
        "  {} stale (content changed, flagged for re-review)",
        summary.stale
    );
    println!(
        "  {} chunks without prior labels (skipped)",
        summary.not_found
    );
}

/// CLI entry point for `style-agent replay-labels`.
pub fn run(config: &Config) -> anyhow::Result<()> {
    let db_path = config.db_path();
    let store = VectorStore::new(&db_path)?;
    store.ensure_collection()?;

    let labels_path = config.labels_dir().join("labels.jsonl");
    let label_store = LabelStore::new(labels_path.clone());

    println!("Loading labels from {}...", labels_path.display());

    let summary = replay(&store, &label_store)?;
    print_summary(&summary);

    Ok(())
}
