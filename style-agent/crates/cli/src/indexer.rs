use std::path::Path;

use style_agent_core::bats_chunker::chunk_bats_file;
use style_agent_core::chunker::chunk_file;
use style_agent_core::classifier::Classifier;
use style_agent_core::embedder::Embedder;
use style_agent_core::labeler::{classify_chunk, ChunkClassification, ChunkData as LabelChunkData};
use style_agent_core::store::{IndexedChunk, VectorStore};
use uuid::Uuid;

use crate::config::Config;
use crate::walker;

const EMBED_BATCH_SIZE: usize = 32;

/// Index a single repository from a local path.
///
/// Files are consumed one at a time so their content is freed after chunking.
/// Chunks stream through a fixed-size batch buffer: when the buffer fills,
/// the batch is embedded, upserted, and labeled before continuing.  This
/// bounds peak memory to roughly one file + one batch instead of the entire
/// corpus.
#[allow(clippy::too_many_arguments)]
pub async fn index_repo(
    repo_path: &Path,
    repo_name: &str,
    subpaths: Option<&[String]>,
    exclude_dirs: &[String],
    store: &VectorStore,
    embedder: &Embedder,
    classifier: &mut Option<Classifier>,
    confidence_threshold: f32,
) -> anyhow::Result<usize> {
    tracing::info!(repo = %repo_name, path = %repo_path.display(), "Starting repository indexing");

    anyhow::ensure!(
        repo_path.is_dir(),
        "Repository path does not exist or is not a directory: {}",
        repo_path.display()
    );

    let files = walker::walk_repo(repo_path, subpaths, exclude_dirs)?;
    let file_count = files.len();
    tracing::info!(file_count, "Discovered source files");

    if files.is_empty() {
        println!("  No source files found in {}", repo_path.display());
        return Ok(0);
    }

    // Delete existing chunks for this repo (for clean re-indexing)
    store.delete_repo(repo_name)?;

    // Stream chunks through a fixed-size batch buffer to bound memory.
    // `files` is consumed via `into_iter()` so each file's content is freed
    // after its chunks have been extracted.
    let mut batch_buf: Vec<ChunkData> = Vec::with_capacity(EMBED_BATCH_SIZE);
    let mut total_chunks = 0usize;
    let mut ml_count = 0usize;
    let mut heuristic_count = 0usize;

    for file in files {
        let chunks = match file.language.as_str() {
            "rust" => chunk_file(&file.content),
            "bats" | "bash" => chunk_bats_file(&file.content),
            _ => continue,
        };

        for chunk in chunks {
            batch_buf.push(ChunkData {
                content: chunk.content,
                chunk_type: chunk.chunk_type,
                entity_name: chunk.entity_name,
                line_start: chunk.line_start,
                line_end: chunk.line_end,
                file_path: file.path.clone(),
                repo: repo_name.to_string(),
                module_path: file.module_path.clone(),
                language: file.language.clone(),
            });

            if batch_buf.len() >= EMBED_BATCH_SIZE {
                let flushed = flush_batch(
                    &mut batch_buf,
                    store,
                    embedder,
                    classifier,
                    confidence_threshold,
                    &mut ml_count,
                    &mut heuristic_count,
                )
                .await?;
                total_chunks += flushed;
                tracing::info!(progress = total_chunks, "Embedded batch");
            }
        }
        // `file` (including its content String) is dropped here
    }

    // Flush remaining partial batch
    if !batch_buf.is_empty() {
        let flushed = flush_batch(
            &mut batch_buf,
            store,
            embedder,
            classifier,
            confidence_threshold,
            &mut ml_count,
            &mut heuristic_count,
        )
        .await?;
        total_chunks += flushed;
    }

    if total_chunks == 0 {
        println!("  No code chunks extracted from {}", repo_path.display());
        return Ok(0);
    }

    let source_info = if classifier.is_some() {
        format!(" (ml: {ml_count}, heuristic: {heuristic_count})")
    } else {
        String::new()
    };
    println!("  Indexed {total_chunks} chunks from {file_count} files in {repo_name}{source_info}");
    Ok(total_chunks)
}

/// Embed, upsert, and label a batch of chunks, then clear the buffer.
///
/// Returns the number of chunks processed.
#[allow(clippy::too_many_arguments)]
async fn flush_batch(
    batch: &mut Vec<ChunkData>,
    store: &VectorStore,
    embedder: &Embedder,
    classifier: &mut Option<Classifier>,
    confidence_threshold: f32,
    ml_count: &mut usize,
    heuristic_count: &mut usize,
) -> anyhow::Result<usize> {
    let count = batch.len();

    let text_batch: Vec<String> = batch
        .iter()
        .map(|c| build_embed_text(&c.content, &c.file_path, c.entity_name.as_deref()))
        .collect();

    let embeddings = embedder.embed_document_batch(text_batch).await?;

    let indexed: Vec<IndexedChunk> = batch
        .iter()
        .zip(embeddings)
        .map(|(data, embedding)| IndexedChunk {
            id: Uuid::new_v4().to_string(),
            embedding,
            content: data.content.clone(),
            file_path: data.file_path.clone(),
            repo: data.repo.clone(),
            chunk_type: data.chunk_type.clone(),
            entity_name: data.entity_name.clone(),
            module_path: data.module_path.clone(),
            language: data.language.clone(),
            line_start: data.line_start,
            line_end: data.line_end,
        })
        .collect();

    let label_updates: Vec<_> = batch
        .iter()
        .zip(indexed.iter())
        .map(|(chunk, ic)| {
            let label_data = LabelChunkData {
                content: chunk.content.clone(),
                file_path: chunk.file_path.clone(),
                chunk_type: chunk.chunk_type.clone(),
                entity_name: chunk.entity_name.clone().unwrap_or_default(),
            };
            let (cls, source) =
                classify_with_fallback(classifier, &label_data, confidence_threshold);
            match source.as_str() {
                "ml" => *ml_count += 1,
                _ => *heuristic_count += 1,
            }
            (ic.id.clone(), cls, source)
        })
        .collect();

    store.upsert_chunks(indexed)?;
    store.set_labels(&label_updates)?;

    batch.clear();
    Ok(count)
}

/// Attempt to load the ONNX classifier from the configured model directory.
///
/// Returns `None` (with a warning log) if the model files don't exist or fail
/// to load, allowing the caller to fall back to heuristic-only labeling.
pub(crate) fn try_load_classifier(config: &Config) -> Option<Classifier> {
    let model_dir = config.model_dir();
    if !model_dir.join("model.onnx").exists() {
        tracing::warn!(
            path = %model_dir.display(),
            "ONNX model not found, using heuristic labels only"
        );
        return None;
    }
    match Classifier::load(&model_dir) {
        Ok(c) => {
            tracing::info!(
                classes = c.labels().len(),
                path = %model_dir.display(),
                "ONNX classifier loaded"
            );
            Some(c)
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Failed to load ONNX classifier, using heuristic labels only"
            );
            None
        }
    }
}

/// Classify a chunk using ML (if available and confident) or heuristic fallback.
///
/// Always runs the heuristic to get layer and uses tags, then overrides the
/// primary label with the ML prediction when confidence exceeds the threshold.
///
/// Returns `(classification, label_source)` where source is `"ml"` or `"heuristic"`.
pub(crate) fn classify_with_fallback(
    classifier: &mut Option<Classifier>,
    chunk: &LabelChunkData,
    threshold: f32,
) -> (ChunkClassification, String) {
    // Always run heuristic for layer + uses (ML only predicts primary label)
    let mut cls = classify_chunk(chunk);
    let mut source = "heuristic".to_string();

    if let Some(cl) = classifier.as_mut() {
        match cl.classify(&chunk.content) {
            Ok(pred) if pred.confidence >= threshold => {
                cls.primary_label = Some(pred.label);
                cls.primary_confidence = pred.confidence;
                cls.primary_signals = vec!["onnx_classifier".to_string()];
                source = "ml".to_string();
            }
            Ok(pred) => {
                tracing::debug!(
                    ml_label = %pred.label,
                    ml_confidence = %pred.confidence,
                    threshold,
                    "ML confidence below threshold, using heuristic"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "Classifier inference error, using heuristic");
            }
        }
    }

    (cls, source)
}

/// Intermediate struct holding chunk data before embedding.
struct ChunkData {
    content: String,
    chunk_type: String,
    entity_name: Option<String>,
    line_start: usize,
    line_end: usize,
    file_path: String,
    repo: String,
    module_path: String,
    language: String,
}

/// Build a text representation for embedding that includes file context.
fn build_embed_text(content: &str, file_path: &str, entity_name: Option<&str>) -> String {
    let mut text = String::new();
    text.push_str("// file: ");
    text.push_str(file_path);
    text.push('\n');
    if let Some(name) = entity_name {
        text.push_str("// entity: ");
        text.push_str(name);
        text.push('\n');
    }
    text.push_str(content);
    text
}
