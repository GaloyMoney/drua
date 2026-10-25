use std::sync::{Arc, Mutex};

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

/// nomic-embed-text task prefix for indexing documents.
const SEARCH_DOCUMENT_PREFIX: &str = "search_document: ";
/// nomic-embed-text task prefix for search queries.
const SEARCH_QUERY_PREFIX: &str = "search_query: ";

/// nomic-embed-text has 8192 token context. Code tokenizes at ~2-3 chars/token.
/// 6000 bytes ≈ 2000-3000 tokens — safe margin under the 8192 limit.
const MAX_BYTES: usize = 6000;

/// Cut `text` to at most `limit` bytes, backing off to the nearest UTF-8
/// char boundary. Slicing at a raw byte offset panics when it lands inside
/// a multi-byte character (e.g. `—`, `−`, `≫`), which killed every
/// `library.embed` attempt for documents with one straddling byte 6000.
fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// In-process embedding via fastembed ONNX (nomic-embed-text-v1.5 quantized).
#[derive(Clone)]
pub struct Embedder {
    model: Arc<Mutex<TextEmbedding>>,
}

impl std::fmt::Debug for Embedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Embedder")
            .field("model", &"NomicEmbedTextV15Q")
            .finish()
    }
}

impl Embedder {
    /// Initialise the fastembed model (downloads on first use).
    pub fn new() -> anyhow::Result<Self> {
        let model = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::NomicEmbedTextV15Q).with_show_download_progress(true),
        )?;
        Ok(Self {
            model: Arc::new(Mutex::new(model)),
        })
    }

    /// Embed a document for indexing (prepends `search_document:` prefix).
    pub async fn embed_document(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let input = truncate(&format!("{SEARCH_DOCUMENT_PREFIX}{text}"), MAX_BYTES);
        let model = self.model.clone();
        tokio::task::spawn_blocking(move || {
            let mut model = model.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
            let mut results = model.embed(vec![input], None)?;
            results
                .pop()
                .ok_or_else(|| anyhow::anyhow!("No embedding returned"))
        })
        .await?
    }

    /// Embed a query for search (prepends `search_query:` prefix).
    pub async fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let input = truncate(&format!("{SEARCH_QUERY_PREFIX}{text}"), MAX_BYTES);
        let model = self.model.clone();
        tokio::task::spawn_blocking(move || {
            let mut model = model.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
            let mut results = model.embed(vec![input], None)?;
            results
                .pop()
                .ok_or_else(|| anyhow::anyhow!("No embedding returned"))
        })
        .await?
    }

    /// Embed a batch of documents for indexing (prepends `search_document:` prefix to each).
    ///
    /// NomicEmbedTextV15Q uses dynamic quantization which requires all items
    /// in a single forward pass (`batch_size = None`).  The caller controls
    /// peak memory by sending small batches.
    pub async fn embed_document_batch(&self, texts: Vec<String>) -> anyhow::Result<Vec<Vec<f32>>> {
        let prefixed: Vec<String> = texts
            .into_iter()
            .map(|t| truncate(&format!("{SEARCH_DOCUMENT_PREFIX}{t}"), MAX_BYTES))
            .collect();
        let model = self.model.clone();
        tokio::task::spawn_blocking(move || {
            let mut model = model.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
            model.embed(prefixed, None)
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_text_is_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_ascii_cuts_at_limit() {
        assert_eq!(truncate("abcdef", 3), "abc");
    }

    #[test]
    fn truncate_backs_off_to_char_boundary() {
        // Production shape: a 3-byte char occupying bytes 5998..6001.
        for ch in ['\u{2014}', '\u{2212}', '\u{226b}'] {
            let text = format!("{}{ch}tail", "a".repeat(5998));
            let out = truncate(&text, MAX_BYTES);
            assert_eq!(out, "a".repeat(5998), "char {ch:?}");
        }
    }

    #[test]
    fn truncate_limit_inside_leading_multibyte_char() {
        assert_eq!(truncate("\u{2014}abc", 1), "");
    }
}
