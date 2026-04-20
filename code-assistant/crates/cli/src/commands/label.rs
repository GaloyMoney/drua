use std::collections::HashMap;

use code_assistant_core::labeler::ChunkData as LabelChunkData;
use code_assistant_core::store::VectorStore;

use crate::config::Config;
use crate::indexer::{classify_with_fallback, try_load_classifier};

/// Apply labels to all indexed chunks using the ONNX classifier (with
/// heuristic fallback). Human labels are never overwritten.
pub fn run(config: &Config) -> anyhow::Result<()> {
    let db_path = config.db_path();
    let store = VectorStore::new(&db_path)?;
    store.ensure_collection()?;

    let mut classifier = try_load_classifier(config);
    let threshold = config.services.classifier_confidence_threshold;

    if classifier.is_some() {
        println!("Using ONNX classifier (threshold: {threshold:.0}%)");
    } else {
        println!("No ONNX model available, using heuristic labels only");
    }

    println!("Scrolling all chunks...");
    let chunks = store.scroll_all_chunks()?;
    let total = chunks.len();
    println!("Found {total} chunks to label.");

    if total == 0 {
        println!("Nothing to label. Run 'bootstrap' or 'index' first.");
        return Ok(());
    }

    // Apply classifier to each chunk, respecting label priority
    let mut updates = Vec::with_capacity(total);
    let mut label_counts: HashMap<String, usize> = HashMap::new();
    let mut source_counts: HashMap<String, usize> = HashMap::new();
    let mut unlabeled = 0usize;
    let mut skipped_human = 0usize;
    let mut total_confidence = 0.0f64;
    let mut labeled_count = 0usize;
    let mut layer_counts: HashMap<String, usize> = HashMap::new();
    let mut uses_counts: HashMap<String, usize> = HashMap::new();

    for chunk in &chunks {
        // Never overwrite human labels (human > ml > heuristic)
        if chunk.label_source == "human" || chunk.label_source == "human_stale" {
            skipped_human += 1;
            continue;
        }

        let chunk_data = LabelChunkData {
            content: chunk.content.clone(),
            file_path: chunk.file_path.clone(),
            chunk_type: chunk.chunk_type.clone(),
            entity_name: chunk.entity_name.clone().unwrap_or_default(),
        };

        let (cls, source) = classify_with_fallback(&mut classifier, &chunk_data, threshold);

        if let Some(ref label) = cls.primary_label {
            labeled_count += 1;
            total_confidence += cls.primary_confidence as f64;
            *label_counts.entry(label.clone()).or_insert(0) += 1;
        } else {
            unlabeled += 1;
        }

        if let Some(ref layer) = cls.layer {
            *layer_counts.entry(layer.clone()).or_insert(0) += 1;
        }
        for u in &cls.uses {
            *uses_counts.entry(u.clone()).or_insert(0) += 1;
        }
        *source_counts.entry(source.clone()).or_insert(0) += 1;

        updates.push((chunk.id.clone(), cls, source));
    }

    // Write labels to the store
    println!("Writing labels...");
    store.set_labels(&updates)?;

    // Print summary
    println!("\n--- Label Summary ---");
    println!("Total chunks:    {total}");
    println!("Human (kept):    {skipped_human}");
    println!("Labeled:         {labeled_count}");
    println!("Unlabeled:       {unlabeled}");

    if labeled_count > 0 {
        let avg_conf = total_confidence / labeled_count as f64;
        println!("Avg confidence:  {avg_conf:.2}");
    }

    if !source_counts.is_empty() {
        println!("\nLabel source:");
        let mut sorted_sources: Vec<_> = source_counts.into_iter().collect();
        sorted_sources.sort_by_key(|x| std::cmp::Reverse(x.1));
        for (source, count) in &sorted_sources {
            println!("  {source:<15} {count}");
        }
    }

    println!("\nPrimary label counts:");
    let mut sorted_labels: Vec<_> = label_counts.into_iter().collect();
    sorted_labels.sort_by_key(|x| std::cmp::Reverse(x.1));
    for (label, count) in &sorted_labels {
        println!("  {label:<25} {count}");
    }

    println!("\nLayer counts:");
    let mut sorted_layers: Vec<_> = layer_counts.into_iter().collect();
    sorted_layers.sort_by_key(|x| std::cmp::Reverse(x.1));
    for (layer, count) in &sorted_layers {
        println!("  {layer:<25} {count}");
    }

    println!("\nUses counts:");
    let mut sorted_uses: Vec<_> = uses_counts.into_iter().collect();
    sorted_uses.sort_by_key(|x| std::cmp::Reverse(x.1));
    for (u, count) in &sorted_uses {
        println!("  {u:<25} {count}");
    }

    println!("\nDone.");
    Ok(())
}
