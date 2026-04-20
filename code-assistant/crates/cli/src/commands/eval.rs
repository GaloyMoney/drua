use std::collections::HashMap;

use code_assistant_core::label_store::{content_hash, LabelStore};
use code_assistant_core::labeler::{classify_chunk, ChunkData, KNOWN_PRIMARY_LABELS};

use crate::config::Config;

/// Evaluate the heuristic classifier against human labels.
///
/// Loads human-reviewed labels from JSONL, splits deterministically into
/// train/test (80/20 by content hash), runs the heuristic classifier on the
/// test set, and reports per-label accuracy plus confusion stats.
pub fn run(config: &Config) -> anyhow::Result<()> {
    let label_store = LabelStore::new(config.labels_dir().join("labels.jsonl"));
    let all_labels = label_store.load_all()?;

    if all_labels.is_empty() {
        println!("No human labels found. Run 'review' or 'telegram-review' first.");
        return Ok(());
    }

    // Split into train/test deterministically using the first byte of content_hash.
    // content_hash is a SHA-256 hex digest; we parse the first 2 hex chars as u8.
    // Values 0..204 (80%) → train, 204..255 (20%) → test.
    let mut train_count = 0usize;
    let mut test_records = Vec::new();

    for record in all_labels.values() {
        let human_label = match record.labels.first() {
            Some(l) => l.clone(),
            None => continue, // skip unlabeled
        };

        let hash = content_hash(&record.content);
        let first_byte = u8::from_str_radix(&hash[..2], 16).unwrap_or(0);

        if first_byte >= 204 {
            // ~20% test set
            test_records.push((record.clone(), human_label));
        } else {
            train_count += 1;
        }
    }

    println!(
        "Train: {} examples | Test: {} examples\n",
        train_count,
        test_records.len()
    );

    if test_records.is_empty() {
        println!("Not enough data for a test split yet. Keep labeling!");
        return Ok(());
    }

    // Run heuristic classifier on test set and collect results.
    let mut per_label_total: HashMap<String, usize> = HashMap::new();
    let mut per_label_correct: HashMap<String, usize> = HashMap::new();
    let mut mismatches: HashMap<(String, String), usize> = HashMap::new();
    let mut total_correct = 0usize;

    for (record, human_label) in &test_records {
        let chunk_data = ChunkData {
            content: record.content.clone(),
            file_path: record.key.file_path.clone(),
            chunk_type: record.key.chunk_type.clone(),
            entity_name: record.key.entity_name.clone(),
        };

        let classification = classify_chunk(&chunk_data);
        let predicted = classification
            .primary_label
            .unwrap_or_else(|| "(none)".to_string());

        *per_label_total.entry(human_label.clone()).or_default() += 1;

        if predicted == *human_label {
            total_correct += 1;
            *per_label_correct.entry(human_label.clone()).or_default() += 1;
        } else {
            *mismatches
                .entry((predicted.clone(), human_label.clone()))
                .or_default() += 1;
        }
    }

    // Print per-label accuracy table.
    println!(
        "{:<25} {:>5}  {:>7}  {:>5}",
        "Label", "Test", "Correct", "Acc%"
    );
    println!("{}", "-".repeat(50));

    for label in KNOWN_PRIMARY_LABELS {
        let total = per_label_total.get(*label).copied().unwrap_or(0);
        if total == 0 {
            continue;
        }
        let correct = per_label_correct.get(*label).copied().unwrap_or(0);
        let pct = (correct as f64 / total as f64) * 100.0;
        println!("{:<25} {:>5}  {:>7}  {:>5.1}%", label, total, correct, pct);
    }

    // Show any labels not in KNOWN_PRIMARY_LABELS.
    for (label, &total) in &per_label_total {
        if !KNOWN_PRIMARY_LABELS.contains(&label.as_str()) {
            let correct = per_label_correct.get(label).copied().unwrap_or(0);
            let pct = (correct as f64 / total as f64) * 100.0;
            println!("{:<25} {:>5}  {:>7}  {:>5.1}%", label, total, correct, pct);
        }
    }

    let total_test = test_records.len();
    let pct = (total_correct as f64 / total_test as f64) * 100.0;
    println!("\nOverall accuracy: {total_correct}/{total_test} ({pct:.1}%)");

    // Print mismatches sorted by frequency.
    if !mismatches.is_empty() {
        let mut mismatch_list: Vec<_> = mismatches.into_iter().collect();
        mismatch_list.sort_by_key(|x| std::cmp::Reverse(x.1));

        println!("\nMismatches:");
        for ((predicted, human), count) in &mismatch_list {
            println!("  heuristic={predicted} \u{2192} human={human}  ({count}x)");
        }
    }

    Ok(())
}
