pub mod config;
mod entity;
pub mod error;
pub(crate) mod repo;
mod store;

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tracing::instrument;

pub use entity::{Memory, NewMemory, SearchResult};
pub use error::*;
use repo::*;
use store::SearchStore;

pub use config::MemoryConfig;

use crate::primitives::*;
use code_assistant_core::embedder::Embedder;

/// Parameters for storing a new memory.
pub struct StoreMemoryParams {
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
}

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

#[derive(Clone)]
pub struct Memories {
    repo: MemoryRepo,
    search_store: SearchStore,
    embedder: Arc<Embedder>,
    decay_half_life_days: f64,
}

impl std::fmt::Debug for Memories {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Memories").finish_non_exhaustive()
    }
}

impl Memories {
    pub fn new(pool: &sqlx::PgPool, embedder: Arc<Embedder>, decay_half_life_days: f64) -> Self {
        let repo = MemoryRepo::new(pool);
        let search_store = SearchStore::new(pool);
        tracing::info!("Memory service ready (PostgreSQL)");
        Self {
            repo,
            search_store,
            embedder,
            decay_half_life_days,
        }
    }

    // ── Store ───────────────────────────────────────────────────────

    #[instrument(name = "memory.store", skip_all, fields(title = %params.title))]
    pub async fn store(&self, params: StoreMemoryParams) -> Result<Memory, MemoryError> {
        let new_memory = NewMemory::builder()
            .title(params.title.clone())
            .content(params.content.clone())
            .tags(params.tags.clone())
            .build()
            .expect("Could not build new memory");
        let memory_id = new_memory.id;

        let memory = self.repo.create(new_memory).await?;

        self.search_store
            .insert_search_data(memory_id, &params)
            .await?;

        // Generate embedding asynchronously.
        let text = format!("{}\n\n{}", params.title, params.content);
        let embedder = self.embedder.clone();
        let search_store = self.search_store.clone();
        tokio::task::spawn(async move {
            match embedder.embed_document(&text).await {
                Ok(embedding) => {
                    if let Err(e) = search_store.upsert_embedding(memory_id, &embedding).await {
                        tracing::warn!(error = %e, "Failed to store embedding");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to generate embedding");
                }
            }
        });

        tracing::info!(id = %memory.id, "Memory stored");
        Ok(memory)
    }

    // ── List ────────────────────────────────────────────────────────

    #[instrument(name = "memory.list", skip_all)]
    pub async fn list(&self, limit: usize) -> Result<Vec<Memory>, MemoryError> {
        let ids = self.search_store.list_ids(limit as i64).await?;
        let mut memories = Vec::with_capacity(ids.len());
        for id in ids {
            match self.repo.find_by_id(id).await {
                Ok(memory) => memories.push(memory),
                Err(e) => {
                    tracing::warn!(id = %id, error = %e, "Failed to hydrate memory, skipping");
                }
            }
        }
        Ok(memories)
    }

    // ── Get ─────────────────────────────────────────────────────────

    #[instrument(name = "memory.find_by_id", skip_all)]
    pub async fn find_by_id(&self, id: MemoryId) -> Result<Memory, MemoryError> {
        Ok(self.repo.find_by_id(id).await?)
    }

    #[instrument(name = "memory.find_by_id_prefix", skip_all)]
    pub async fn find_by_id_prefix(&self, prefix: &str) -> Result<Option<Memory>, MemoryError> {
        let id = self.search_store.find_id_by_prefix(prefix).await?;
        match id {
            Some(id) => Ok(Some(self.repo.find_by_id(id).await?)),
            None => Ok(None),
        }
    }

    // ── Search ──────────────────────────────────────────────────────

    /// Hybrid search: FTS + vector with Reciprocal Rank Fusion.
    #[instrument(name = "memory.search", skip_all, fields(%query))]
    pub async fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, MemoryError> {
        let fetch_limit = (limit * 3) as i64;

        // 1. FTS keyword search.
        let fts_results = self.search_store.search_fts(query, fetch_limit).await?;

        // 2. Vector search (if embeddings exist).
        let vec_results = if self.search_store.has_embeddings().await? {
            match self.embedder.embed_query(query).await {
                Ok(query_embedding) => {
                    self.search_store
                        .search_vector(&query_embedding, fetch_limit)
                        .await?
                }
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
        let mut scores: HashMap<MemoryId, f64> = HashMap::new();

        for (rank, result) in fts_results.iter().enumerate() {
            *scores.entry(result.id).or_default() += 1.0 / (k + rank as f64 + 1.0);
        }
        for (rank, result) in vec_results.iter().enumerate() {
            *scores.entry(result.id).or_default() += 1.0 / (k + rank as f64 + 1.0);
        }

        // Sort by RRF score descending.
        let mut ranked: Vec<(MemoryId, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Fetch full memories and build results with decay scoring.
        let mut results = Vec::new();
        let half_life = self.decay_half_life_days;

        for (id, rrf_score) in &ranked {
            let m = match self.repo.find_by_id(*id).await {
                Ok(m) => m,
                Err(_) => continue,
            };

            let meta =
                self.search_store
                    .get_access_metadata(*id)
                    .await
                    .unwrap_or(store::AccessMetadata {
                        last_accessed: None,
                    });

            let df = if m.pinned {
                1.0
            } else {
                let accessed = meta.last_accessed.unwrap_or(m.created_at());
                decay_factor(accessed, half_life)
            };

            // Filter below minimum strength.
            if !m.pinned && df < DECAY_MIN_STRENGTH {
                continue;
            }

            let adjusted_score = rrf_score * df;
            results.push(SearchResult {
                id: m.id,
                title: m.title.clone(),
                content: m.content.clone(),
                tags: m.tags.clone(),
                score: adjusted_score,
                decay_factor: df,
                pinned: m.pinned,
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
        let returned_ids: Vec<MemoryId> = results.iter().map(|r| r.id).collect();
        if let Err(e) = self.search_store.record_access(&returned_ids).await {
            tracing::warn!(error = %e, "Failed to record access for search results");
        }

        Ok(results)
    }
}
