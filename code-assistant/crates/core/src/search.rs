use crate::embedder::Embedder;
use crate::request_log::{RequestLogEntry, StatsResponse};
use crate::store::{AntiPatternResult, LabelOriginCounts, SearchResult, VectorStore};

/// High-level search combining the embedder and vector store.
#[derive(Debug)]
pub struct SearchEngine {
    embedder: Embedder,
    store: VectorStore,
}

impl SearchEngine {
    pub fn new(embedder: Embedder, store: VectorStore) -> Self {
        Self { embedder, store }
    }

    /// Embed the query text and search for similar code chunks.
    #[allow(clippy::too_many_arguments)]
    pub async fn search(
        &self,
        query: &str,
        limit: u64,
        repo: Option<&str>,
        language: Option<&str>,
        label: Option<&str>,
        layer: Option<&str>,
        uses: Option<&str>,
    ) -> anyhow::Result<Vec<SearchResult>> {
        let embedding = self.embedder.embed_query(query).await?;
        self.store
            .search(embedding, limit, repo, language, label, layer, uses)
    }

    /// Search anti-patterns by code similarity (code-to-code matching).
    pub async fn search_anti_patterns_by_code(
        &self,
        code: &str,
        limit: u64,
    ) -> anyhow::Result<Vec<AntiPatternResult>> {
        let embedding = self.embedder.embed_query(code).await?;
        self.store.search_anti_patterns_by_code(embedding, limit)
    }

    /// Search anti-patterns by semantic context (review comment + code).
    pub async fn search_anti_patterns(
        &self,
        query: &str,
        limit: u64,
    ) -> anyhow::Result<Vec<AntiPatternResult>> {
        let embedding = self.embedder.embed_query(query).await?;
        self.store.search_anti_patterns(embedding, limit)
    }

    /// Return the number of stored anti-patterns.
    pub fn anti_pattern_count(&self) -> anyhow::Result<u64> {
        self.store.anti_pattern_count()
    }

    /// Ensure anti-pattern tables exist.
    pub fn ensure_anti_pattern_tables(&self) -> anyhow::Result<()> {
        self.store.ensure_anti_pattern_tables()
    }

    /// Insert a request log entry (fire-and-forget).
    pub fn log_request(&self, entry: &RequestLogEntry) -> anyhow::Result<()> {
        self.store.log_request(entry)
    }

    /// Query aggregated stats from the request log.
    pub fn query_stats(&self, low_score_threshold: f64) -> anyhow::Result<StatsResponse> {
        self.store.query_stats(low_score_threshold)
    }

    /// Count labelled chunks grouped by their origin.
    pub fn label_origin_counts(&self) -> anyhow::Result<LabelOriginCounts> {
        self.store.label_origin_counts()
    }
}
