use std::sync::{Arc, Mutex};

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

/// nomic-embed-text task prefix for indexing documents.
const SEARCH_DOCUMENT_PREFIX: &str = "search_document: ";
/// nomic-embed-text task prefix for search queries.
const SEARCH_QUERY_PREFIX: &str = "search_query: ";

/// nomic-embed-text has 8192 token context. Code tokenizes at ~2-3 chars/token.
/// 6000 chars ≈ 2000-3000 tokens — safe margin under the 8192 limit.
const MAX_CHARS: usize = 6000;

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        text.to_string()
    } else {
        text[..limit].to_string()
    }
}

/// In-process embedding via fastembed ONNX (nomic-embed-text-v1.5 quantized).
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
        let input = truncate(&format!("{SEARCH_DOCUMENT_PREFIX}{text}"), MAX_CHARS);
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
        let input = truncate(&format!("{SEARCH_QUERY_PREFIX}{text}"), MAX_CHARS);
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
    pub async fn embed_document_batch(&self, texts: Vec<String>) -> anyhow::Result<Vec<Vec<f32>>> {
        let prefixed: Vec<String> = texts
            .into_iter()
            .map(|t| truncate(&format!("{SEARCH_DOCUMENT_PREFIX}{t}"), MAX_CHARS))
            .collect();
        let model = self.model.clone();
        tokio::task::spawn_blocking(move || {
            let mut model = model.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
            model.embed(prefixed, None)
        })
        .await?
    }
}
