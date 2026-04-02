pub mod config;
mod entity;
pub mod error;
pub(crate) mod repo;
mod store;

use std::collections::HashMap;
use std::sync::Arc;

use tracing::instrument;

pub use entity::{NewReport, Report, SearchResult};
pub use error::*;
use repo::*;
use store::SearchStore;

pub use config::ReportConfig;

use crate::primitives::*;
use code_assistant_core::embedder::Embedder;

/// Parameters for storing a new report.
pub struct StoreReportParams {
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
}

#[derive(Clone)]
pub struct Reports {
    repo: ReportRepo,
    search_store: SearchStore,
    embedder: Arc<Embedder>,
}

impl std::fmt::Debug for Reports {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reports").finish_non_exhaustive()
    }
}

impl Reports {
    pub fn new(pool: &sqlx::PgPool, embedder: Arc<Embedder>) -> Self {
        let repo = ReportRepo::new(pool);
        let search_store = SearchStore::new(pool);
        tracing::info!("Report service ready (PostgreSQL)");
        Self {
            repo,
            search_store,
            embedder,
        }
    }

    // ── Store ───────────────────────────────────────────────────────

    #[instrument(name = "report.store", skip_all, fields(title = %params.title))]
    pub async fn store(&self, params: StoreReportParams) -> Result<Report, ReportError> {
        let new_report = NewReport::builder()
            .title(params.title.clone())
            .content(params.content.clone())
            .tags(params.tags.clone())
            .build()
            .expect("Could not build new report");
        let report_id = new_report.id;

        let report = self.repo.create(new_report).await?;

        self.search_store
            .insert_search_data(report_id, &params)
            .await?;

        // Generate embedding asynchronously.
        let text = format!("{}\n\n{}", params.title, params.content);
        let embedder = self.embedder.clone();
        let search_store = self.search_store.clone();
        tokio::task::spawn(async move {
            match embedder.embed_document(&text).await {
                Ok(embedding) => {
                    if let Err(e) = search_store.upsert_embedding(report_id, &embedding).await {
                        tracing::warn!(error = %e, "Failed to store embedding");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to generate embedding");
                }
            }
        });

        tracing::info!(id = %report.id, "Report stored");
        Ok(report)
    }

    // ── List ────────────────────────────────────────────────────────

    #[instrument(name = "report.list", skip_all)]
    pub async fn list(&self, limit: usize) -> Result<Vec<Report>, ReportError> {
        let ids = self.search_store.list_ids(limit as i64).await?;
        let mut reports = Vec::with_capacity(ids.len());
        for id in ids {
            match self.repo.find_by_id(id).await {
                Ok(report) => reports.push(report),
                Err(e) => {
                    tracing::warn!(id = %id, error = %e, "Failed to hydrate report, skipping");
                }
            }
        }
        Ok(reports)
    }

    // ── Get ─────────────────────────────────────────────────────────

    #[instrument(name = "report.find_by_id", skip_all)]
    pub async fn find_by_id(&self, id: ReportId) -> Result<Report, ReportError> {
        Ok(self.repo.find_by_id(id).await?)
    }

    #[instrument(name = "report.find_by_id_prefix", skip_all)]
    pub async fn find_by_id_prefix(&self, prefix: &str) -> Result<Option<Report>, ReportError> {
        let id = self.search_store.find_id_by_prefix(prefix).await?;
        match id {
            Some(id) => Ok(Some(self.repo.find_by_id(id).await?)),
            None => Ok(None),
        }
    }

    // ── Search ──────────────────────────────────────────────────────

    /// Hybrid search: FTS + vector with Reciprocal Rank Fusion.
    #[instrument(name = "report.search", skip_all, fields(%query))]
    pub async fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, ReportError> {
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
        let mut scores: HashMap<ReportId, f64> = HashMap::new();

        for (rank, result) in fts_results.iter().enumerate() {
            *scores.entry(result.id).or_default() += 1.0 / (k + rank as f64 + 1.0);
        }
        for (rank, result) in vec_results.iter().enumerate() {
            *scores.entry(result.id).or_default() += 1.0 / (k + rank as f64 + 1.0);
        }

        // Sort by RRF score descending.
        let mut ranked: Vec<(ReportId, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // Fetch full reports and build results.
        let mut results = Vec::new();

        for (id, rrf_score) in &ranked {
            let m = match self.repo.find_by_id(*id).await {
                Ok(m) => m,
                Err(_) => continue,
            };

            results.push(SearchResult {
                id: m.id,
                title: m.title.clone(),
                content: m.content.clone(),
                tags: m.tags.clone(),
                score: *rrf_score,
                pinned: m.pinned,
            });
        }

        // Re-sort by score descending.
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);

        Ok(results)
    }
}
