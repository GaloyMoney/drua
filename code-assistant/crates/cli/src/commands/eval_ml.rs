use std::collections::{BTreeMap, HashMap, HashSet};

use code_assistant_core::classifier::Classifier;
use code_assistant_core::label_store::{content_hash, LabelStore};
use code_assistant_core::labeler::{classify_chunk, ChunkData};

use crate::config::Config;

/// Evaluate the ONNX classifier against human labels, with side-by-side
/// heuristic comparison.
///
/// Uses the same deterministic 80/20 content-hash split as the `eval` command.
pub fn run(config: &Config) -> anyhow::Result<()> {
    let model_dir = config.model_dir();
    if !model_dir.join("model.onnx").exists() {
        anyhow::bail!(
            "No trained model found at {}. Run `make train-all` first.",
            model_dir.display()
        );
    }

    let mut classifier = Classifier::load(&model_dir)?;
    println!(
        "Loaded ONNX classifier: {} classes\n",
        classifier.labels().len()
    );

    let label_store = LabelStore::new(config.labels_dir().join("labels.jsonl"));
    let all_labels = label_store.load_all()?;

    if all_labels.is_empty() {
        println!("No human labels found. Run 'review' or 'telegram-review' first.");
        return Ok(());
    }

    // Split 80/20 by content hash (identical to `eval` command).
    let mut train_count = 0usize;
    let mut test_records = Vec::new();

    for record in all_labels.values() {
        let human_label = match record.labels.first() {
            Some(l) => l.clone(),
            None => continue,
        };

        let hash = content_hash(&record.content);
        let first_byte = u8::from_str_radix(&hash[..2], 16).unwrap_or(0);

        if first_byte >= 204 {
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

    // Collect all unique labels across human + ML predictions.
    let mut all_label_set: HashSet<String> = HashSet::new();
    let threshold = config.services.classifier_confidence_threshold;

    let mut ml_per_label: HashMap<String, Metrics> = HashMap::new();
    let mut heuristic_per_label: HashMap<String, Metrics> = HashMap::new();
    let mut ml_correct = 0usize;
    let mut heuristic_correct = 0usize;
    let mut ml_confusion: HashMap<(String, String), usize> = HashMap::new();
    let mut ml_source_counts: HashMap<&str, usize> = HashMap::new();

    for (record, human_label) in &test_records {
        all_label_set.insert(human_label.clone());

        let chunk_data = ChunkData {
            content: record.content.clone(),
            file_path: record.key.file_path.clone(),
            chunk_type: record.key.chunk_type.clone(),
            entity_name: record.key.entity_name.clone(),
        };

        // ML prediction
        let (ml_pred, ml_source) = match classifier.classify(&record.content) {
            Ok(pred) if pred.confidence >= threshold => (pred.label, "ml"),
            Ok(_pred) => {
                let heur = classify_chunk(&chunk_data);
                (
                    heur.primary_label.unwrap_or_else(|| "(none)".to_string()),
                    "heuristic_fallback",
                )
            }
            Err(_) => {
                let heur = classify_chunk(&chunk_data);
                (
                    heur.primary_label.unwrap_or_else(|| "(none)".to_string()),
                    "heuristic_fallback",
                )
            }
        };
        *ml_source_counts.entry(ml_source).or_default() += 1;
        all_label_set.insert(ml_pred.clone());

        // Heuristic prediction
        let heuristic_cls = classify_chunk(&chunk_data);
        let heuristic_pred = heuristic_cls
            .primary_label
            .unwrap_or_else(|| "(none)".to_string());
        all_label_set.insert(heuristic_pred.clone());

        // ML metrics
        if ml_pred == *human_label {
            ml_correct += 1;
            ml_per_label
                .entry(human_label.clone())
                .or_default()
                .true_positives += 1;
        } else {
            *ml_confusion
                .entry((ml_pred.clone(), human_label.clone()))
                .or_default() += 1;
            ml_per_label
                .entry(human_label.clone())
                .or_default()
                .false_negatives += 1;
            ml_per_label
                .entry(ml_pred.clone())
                .or_default()
                .false_positives += 1;
        }

        // Heuristic metrics
        if heuristic_pred == *human_label {
            heuristic_correct += 1;
            heuristic_per_label
                .entry(human_label.clone())
                .or_default()
                .true_positives += 1;
        } else {
            heuristic_per_label
                .entry(human_label.clone())
                .or_default()
                .false_negatives += 1;
            heuristic_per_label
                .entry(heuristic_pred)
                .or_default()
                .false_positives += 1;
        }
    }

    let total_test = test_records.len();

    // Print ML source breakdown
    println!("ML predictions sourced from:");
    for (source, count) in &ml_source_counts {
        println!("  {source:<25} {count}");
    }
    println!();

    // Print side-by-side per-label metrics.
    let sorted_labels: BTreeMap<&str, ()> =
        all_label_set.iter().map(|l| (l.as_str(), ())).collect();

    println!(
        "{:<25} {:>5}  {:>6} {:>6} {:>6}  {:>6} {:>6} {:>6}",
        "Label", "N", "ML-P%", "ML-R%", "ML-F1", "H-P%", "H-R%", "H-F1"
    );
    println!("{}", "-".repeat(85));

    let zero = Metrics {
        true_positives: 0,
        false_positives: 0,
        false_negatives: 0,
    };

    for label in sorted_labels.keys() {
        let ml_m = ml_per_label.get(*label).unwrap_or(&zero);
        let h_m = heuristic_per_label.get(*label).unwrap_or(&zero);

        // Skip labels with no test examples
        let ml_total = ml_m.true_positives + ml_m.false_negatives;
        let h_total = h_m.true_positives + h_m.false_negatives;
        if ml_total == 0 && h_total == 0 {
            continue;
        }

        let n = ml_m.true_positives + ml_m.false_negatives;

        let ml_p = precision(ml_m);
        let ml_r = recall(ml_m);
        let ml_f1 = f1(ml_p, ml_r);

        let h_p = precision(h_m);
        let h_r = recall(h_m);
        let h_f1 = f1(h_p, h_r);

        println!(
            "{:<25} {:>5}  {:>5.1}% {:>5.1}% {:>5.1}%  {:>5.1}% {:>5.1}% {:>5.1}%",
            label,
            n,
            ml_p * 100.0,
            ml_r * 100.0,
            ml_f1 * 100.0,
            h_p * 100.0,
            h_r * 100.0,
            h_f1 * 100.0,
        );
    }

    let ml_pct = (ml_correct as f64 / total_test as f64) * 100.0;
    let h_pct = (heuristic_correct as f64 / total_test as f64) * 100.0;
    println!(
        "\nOverall accuracy:  ML: {ml_correct}/{total_test} ({ml_pct:.1}%)  |  Heuristic: {heuristic_correct}/{total_test} ({h_pct:.1}%)"
    );

    // Print ML confusion matrix (mismatches only).
    if !ml_confusion.is_empty() {
        let mut mismatch_list: Vec<_> = ml_confusion.into_iter().collect();
        mismatch_list.sort_by(|a, b| b.1.cmp(&a.1));

        println!("\nML Mismatches:");
        for ((predicted, human), count) in &mismatch_list {
            println!("  predicted={predicted} \u{2192} human={human}  ({count}x)");
        }
    }

    Ok(())
}

fn precision(m: &Metrics) -> f64 {
    let denom = m.true_positives + m.false_positives;
    if denom == 0 {
        0.0
    } else {
        m.true_positives as f64 / denom as f64
    }
}

fn recall(m: &Metrics) -> f64 {
    let denom = m.true_positives + m.false_negatives;
    if denom == 0 {
        0.0
    } else {
        m.true_positives as f64 / denom as f64
    }
}

fn f1(p: f64, r: f64) -> f64 {
    if p + r == 0.0 {
        0.0
    } else {
        2.0 * p * r / (p + r)
    }
}

#[derive(Default)]
struct Metrics {
    true_positives: usize,
    false_positives: usize,
    false_negatives: usize,
}
