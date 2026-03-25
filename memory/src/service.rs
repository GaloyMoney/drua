use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};

use style_agent_core::embedder::Embedder;

use crate::config::MemoryConfig;
use crate::error::MemoryError;
use crate::memory::{Memory, NewMemory, SearchResult};
use crate::store::MemoryStore;

/// Minimum strength before a memory is filtered from search results.
const DECAY_MIN_STRENGTH: f64 = 0.05;

/// Compute exponential decay factor based on time since last access.
fn decay_factor(last_accessed: DateTime<Utc>, half_life_days: f64) -> f64 {
    let lambda = (2.0_f64).ln() / half_life_days;
    let days_elapsed = Utc::now()
        .signed_duration_since(last_accessed)
        .num_seconds() as f64
        / 86400.0;
    (-lambda * days_elapsed).exp()
}

/// High-level orchestrator for memory operations.
///
/// Coordinates the SQLite store and shared embedder for hybrid search.
#[derive(Clone)]
pub struct MemoryService {
    store: Arc<MemoryStore>,
    embedder: Arc<Embedder>,
    decay_half_life_days: f64,
}

impl std::fmt::Debug for MemoryService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryService").finish_non_exhaustive()
    }
}

impl MemoryService {
    /// Create a new MemoryService from config and a shared embedder.
    #[tracing::instrument(name = "memory.service.new", skip_all)]
    pub fn new(config: &MemoryConfig, embedder: Arc<Embedder>) -> Result<Self, MemoryError> {
        let store = MemoryStore::new(std::path::Path::new(&config.db_path))?;

        tracing::info!(db_path = %config.db_path, "Memory service ready");
        Ok(Self {
            store: Arc::new(store),
            embedder,
            decay_half_life_days: config.decay_half_life_days,
        })
    }

    // ── Store ───────────────────────────────────────────────────────

    /// Store a new memory.
    #[tracing::instrument(name = "memory.service.store", skip_all, fields(title = %new.title))]
    pub async fn store(&self, new: NewMemory) -> Result<Memory, MemoryError> {
        let now = Utc::now();
        let id = uuid::Uuid::new_v4().to_string();

        let memory = Memory {
            id: id.clone(),
            title: new.title,
            content: new.content,
            tags: new.tags,
            project: new.project,
            source_task: new.source_task,
            source_type: new.source_type,
            created_at: now,
            updated_at: now,
            last_accessed: None,
            access_count: 0,
            pinned: false,
            persistent: new.persistent,
        };

        // Insert into SQLite.
        self.store.insert(&memory)?;

        // Update FTS index.
        let tags_str = memory.tags.join(", ");
        self.store
            .upsert_fts(&id, &memory.title, &memory.content, &tags_str)?;

        // Generate embedding and store vector.
        let text = format!("{}\n\n{}", memory.title, memory.content);
        let embedder = self.embedder.clone();
        let store = self.store.clone();
        let embed_id = id.clone();
        tokio::task::spawn(async move {
            match embedder.embed_document(&text).await {
                Ok(embedding) => {
                    if let Err(e) = store.upsert_embedding(&embed_id, &embedding) {
                        tracing::warn!(error = %e, "Failed to store embedding");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to generate embedding");
                }
            }
        });

        tracing::info!(id = %memory.id, persistent = memory.persistent, "Memory stored");
        Ok(memory)
    }

    // ── List ────────────────────────────────────────────────────────

    /// List memories with optional filters.
    #[tracing::instrument(name = "memory.service.list", skip_all)]
    pub fn list(
        &self,
        project: Option<&str>,
        persistent: Option<bool>,
        limit: usize,
    ) -> Result<Vec<Memory>, MemoryError> {
        self.store.list(project, persistent, limit)
    }

    // ── Search ──────────────────────────────────────────────────────

    /// Hybrid search: FTS + vector with Reciprocal Rank Fusion.
    #[tracing::instrument(name = "memory.service.search", skip_all, fields(query = %query))]
    pub async fn search(
        &self,
        query: &str,
        project: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchResult>, MemoryError> {
        let fetch_limit = limit * 3;

        // 1. FTS keyword search.
        let fts_results = self.store.search_fts(query, fetch_limit)?;

        // 2. Vector search (if embeddings exist).
        let vec_results = if self.store.has_embeddings()? {
            match self.embedder.embed_query(query).await {
                Ok(query_embedding) => self.store.search_vector(&query_embedding, fetch_limit)?,
                Err(e) => {
                    tracing::warn!(error = %e, "Vector search failed, using FTS only");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        // 3. Reciprocal Rank Fusion.
        let k = 60.0_f64;
        let mut scores: HashMap<String, f64> = HashMap::new();

        for (rank, result) in fts_results.iter().enumerate() {
            *scores.entry(result.id.clone()).or_default() += 1.0 / (k + rank as f64 + 1.0);
        }
        for (rank, result) in vec_results.iter().enumerate() {
            *scores.entry(result.id.clone()).or_default() += 1.0 / (k + rank as f64 + 1.0);
        }

        // Sort by RRF score descending.
        let mut ranked: Vec<(String, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Fetch full memories and build results with decay scoring.
        let mut results = Vec::new();
        let half_life = self.decay_half_life_days;

        for (id, rrf_score) in &ranked {
            let m = match self.store.find_by_id(id) {
                Ok(m) => m,
                Err(_) => continue,
            };

            // Project filter.
            if let Some(proj) = project {
                if m.project.as_deref() != Some(proj) {
                    continue;
                }
            }

            let exempt = m.pinned || m.persistent;
            let df = if exempt {
                1.0
            } else {
                let accessed = m.last_accessed.unwrap_or(m.created_at);
                decay_factor(accessed, half_life)
            };

            // Filter below minimum strength.
            if !exempt && df < DECAY_MIN_STRENGTH {
                continue;
            }

            let adjusted_score = rrf_score * df;
            results.push(SearchResult {
                id: m.id.clone(),
                title: m.title.clone(),
                content: m.content.clone(),
                tags: m.tags.clone(),
                project: m.project.clone(),
                score: adjusted_score,
                decay_factor: df,
                pinned: m.pinned,
                persistent: m.persistent,
            });
        }

        // Re-sort by adjusted score descending.
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);

        // Record access for returned memories.
        let returned_ids: Vec<String> = results.iter().map(|r| r.id.clone()).collect();
        if let Err(e) = self.store.record_access(&returned_ids) {
            tracing::warn!(error = %e, "Failed to record access for search results");
        }

        Ok(results)
    }
}
