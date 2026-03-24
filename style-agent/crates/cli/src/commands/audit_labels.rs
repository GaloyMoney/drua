use style_agent_core::classifier::Classifier;
use style_agent_core::label_store::{write_audit_suspects, AuditSuspect, LabelStore};
use style_agent_core::labeler::{classify_chunk, ChunkData};

use crate::config::Config;

/// Run the ONNX classifier over ALL human-labeled data and flag records where
/// the ML prediction disagrees with the human label.
///
/// Outputs a report grouped by confidence tier and writes
/// `labels/audit-suspects.jsonl`.
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

    println!("Auditing {} labeled records...\n", all_labels.len());

    let threshold = config.services.classifier_confidence_threshold;
    let mut suspects: Vec<AuditSuspect> = Vec::new();
    let mut agree_count = 0usize;
    let mut disagree_count = 0usize;
    let mut fallback_count = 0usize;

    for record in all_labels.values() {
        let human_label = match record.labels.first() {
            Some(l) => l.clone(),
            None => continue,
        };

        let chunk_data = ChunkData {
            content: record.content.clone(),
            file_path: record.key.file_path.clone(),
            chunk_type: record.key.chunk_type.clone(),
            entity_name: record.key.entity_name.clone(),
        };

        // ML prediction (with heuristic fallback below threshold)
        let (ml_label, ml_confidence, is_ml) = match classifier.classify(&record.content) {
            Ok(pred) if pred.confidence >= threshold => (pred.label, pred.confidence, true),
            Ok(pred) => {
                // Below threshold — use heuristic fallback
                let heur = classify_chunk(&chunk_data);
                let label = heur.primary_label.unwrap_or_else(|| "(none)".to_string());
                (label, pred.confidence, false)
            }
            Err(_) => {
                let heur = classify_chunk(&chunk_data);
                let label = heur.primary_label.unwrap_or_else(|| "(none)".to_string());
                (label, 0.0, false)
            }
        };

        if !is_ml {
            fallback_count += 1;
        }

        if ml_label == human_label {
            agree_count += 1;
        } else {
            disagree_count += 1;

            let confidence_tier = if ml_confidence >= 0.8 {
                "high"
            } else if ml_confidence >= 0.5 {
                "medium"
            } else {
                "low"
            }
            .to_string();

            suspects.push(AuditSuspect {
                key: record.key.clone(),
                content: record.content.clone(),
                human_label,
                ml_label,
                ml_confidence,
                confidence_tier,
            });
        }
    }

    // Sort suspects by confidence descending (highest confidence disagreements first).
    suspects.sort_by(|a, b| {
        b.ml_confidence
            .partial_cmp(&a.ml_confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Print summary.
    let total = agree_count + disagree_count;
    println!("Results:");
    println!("  Total records:  {total}");
    println!("  Agree:          {agree_count}");
    println!("  Disagree:       {disagree_count}");
    println!("  Heuristic fallback: {fallback_count}");
    println!();

    if suspects.is_empty() {
        println!("No disagreements found — labels look clean!");
        return Ok(());
    }

    // Group by confidence tier.
    let high: Vec<_> = suspects
        .iter()
        .filter(|s| s.confidence_tier == "high")
        .collect();
    let medium: Vec<_> = suspects
        .iter()
        .filter(|s| s.confidence_tier == "medium")
        .collect();
    let low: Vec<_> = suspects
        .iter()
        .filter(|s| s.confidence_tier == "low")
        .collect();

    println!("Disagreements by confidence tier:");
    println!("  High   (>=0.8): {}", high.len());
    println!("  Medium (>=0.5): {}", medium.len());
    println!("  Low    (<0.5):  {}", low.len());
    println!();

    // Print high-confidence disagreements (most likely mislabels).
    if !high.is_empty() {
        println!("--- High-confidence disagreements (likely mislabels) ---");
        for s in &high {
            println!(
                "  human={:<25} ml={:<25} conf={:.2}  {}:{}",
                s.human_label, s.ml_label, s.ml_confidence, s.key.repo, s.key.file_path,
            );
        }
        println!();
    }

    if !medium.is_empty() {
        println!("--- Medium-confidence disagreements ---");
        for s in &medium {
            println!(
                "  human={:<25} ml={:<25} conf={:.2}  {}:{}",
                s.human_label, s.ml_label, s.ml_confidence, s.key.repo, s.key.file_path,
            );
        }
        println!();
    }

    // Write suspects file.
    let suspects_path = config.labels_dir().join("audit-suspects.jsonl");
    write_audit_suspects(&suspects_path, &suspects)?;
    println!(
        "Wrote {} suspects to {}",
        suspects.len(),
        suspects_path.display()
    );
    println!("Run `make review-confused` or `make telegram-confused` to re-verify.");

    Ok(())
}
