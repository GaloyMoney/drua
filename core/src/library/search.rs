use pgvector::Vector;
use sqlx::PgPool;

use super::error::LibraryError;
use super::file::{DocType, SearchableFields};

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub doc_id: uuid::Uuid,
    pub doc_type: DocType,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub score: f64,
}

#[derive(Debug, Clone)]
pub struct GlobalSearchHit {
    pub doc_id: uuid::Uuid,
    pub doc_type: DocType,
    pub workspace_id: uuid::Uuid,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub score: f64,
    /// Populated only for `doc_type = SpaceFile`. Lets callers cite a
    /// hit as `<space_slug>/<relative_path>` instead of the meaningless
    /// nil workspace id space-file rows carry in `library_search_data`.
    pub space_slug: Option<String>,
    pub relative_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LibraryFile {
    pub doc_id: uuid::Uuid,
    pub doc_type: DocType,
    pub workspace_id: uuid::Uuid,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    /// Populated only for `doc_type = SpaceFile`. The space's slug and
    /// the file's path inside `spaces/<slug>/`. Joined from
    /// `space_search_data` + `spaces` so callers can cite a hit as
    /// `<space_slug>/<relative_path>` without a follow-up lookup.
    pub space_slug: Option<String>,
    pub relative_path: Option<String>,
}

impl std::fmt::Display for SearchResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "id: {}\ntitle: {}\n", self.doc_id, self.title)?;
        if !self.tags.is_empty() {
            writeln!(f, "  tags: {}", self.tags.join(", "))?;
        }
        let preview: String = self.content.chars().take(200).collect();
        write!(f, "preview: {}", preview)
    }
}

/// Operates on the `library_search_data` table.
#[derive(Clone)]
pub(super) struct SearchStore {
    pool: PgPool,
}

impl SearchStore {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    #[tracing::instrument(name = "library.search_store.upsert_in_op", skip_all)]
    pub async fn upsert_in_op(
        &self,
        op: &mut impl es_entity::AtomicOperation,
        fields: &SearchableFields,
    ) -> Result<(), sqlx::Error> {
        let tags_json = serde_json::to_value(&fields.tags).unwrap_or_default();
        let doc_type = fields.doc_type.as_str();
        sqlx::query!(
            r#"INSERT INTO library_search_data (doc_id, doc_type, workspace_id, title_text, content_text, tags)
               VALUES ($1, $2, $3, $4, $5, $6)
               ON CONFLICT (doc_id, doc_type) DO UPDATE SET
                   title_text = EXCLUDED.title_text,
                   content_text = EXCLUDED.content_text,
                   tags = EXCLUDED.tags"#,
            fields.doc_id,
            doc_type,
            fields.workspace_id,
            &fields.title,
            &fields.body,
            tags_json,
        )
        .execute(op.as_executor())
        .await?;
        Ok(())
    }

    #[tracing::instrument(name = "library.search_store.set_embedding", skip_all)]
    pub async fn set_embedding(
        &self,
        doc_id: uuid::Uuid,
        doc_type: DocType,
        embedding: Vec<f32>,
    ) -> Result<(), LibraryError> {
        let vec = Vector::from(embedding);
        sqlx::query!(
            "UPDATE library_search_data SET embedding = $1 WHERE doc_id = $2 AND doc_type = $3",
            vec as Vector,
            doc_id,
            doc_type.as_str(),
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Bulk fetch by `doc_id`. Doc type is part of the table's primary
    /// key but not required here — UUIDs don't collide across types in
    /// practice. Space-file rows are joined to `space_search_data` and
    /// `spaces` so each `LibraryFile` carries its `(space_slug, relative_path)`
    /// citation; non-space rows leave both fields `None`.
    #[tracing::instrument(name = "library.search_store.find_by_ids", skip(self, ids))]
    pub async fn find_by_ids(&self, ids: &[uuid::Uuid]) -> Result<Vec<LibraryFile>, LibraryError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query!(
            r#"SELECT lsd.doc_id, lsd.doc_type, lsd.workspace_id,
                      lsd.title_text, lsd.content_text, lsd.tags,
                      s.slug AS "space_slug?",
                      ssd.relative_path AS "relative_path?"
               FROM library_search_data lsd
               LEFT JOIN space_search_data ssd ON ssd.doc_id = lsd.doc_id
               LEFT JOIN spaces s ON s.id = ssd.space_id
               WHERE lsd.doc_id = ANY($1)"#,
            ids,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| LibraryFile {
                doc_id: r.doc_id,
                doc_type: parse_doc_type(&r.doc_type),
                workspace_id: r.workspace_id,
                title: r.title_text,
                body: r.content_text,
                tags: parse_tags(&r.tags),
                space_slug: r.space_slug,
                relative_path: r.relative_path,
            })
            .collect())
    }

    /// Called during workspace cascade deletion.
    #[tracing::instrument(name = "library.search_store.delete_for_workspace_in_op", skip_all)]
    pub async fn delete_for_workspace_in_op(
        &self,
        op: &mut impl es_entity::AtomicOperation,
        workspace_id: uuid::Uuid,
    ) -> Result<(), LibraryError> {
        sqlx::query("DELETE FROM library_search_data WHERE workspace_id = $1")
            .bind(workspace_id)
            .execute(op.as_executor())
            .await?;
        Ok(())
    }

    /// FTS + vector similarity fused via Reciprocal Rank Fusion. Includes
    /// `workspace_id` plus the nil UUID so global-scoped library files
    /// surface alongside workspace-scoped ones.
    #[tracing::instrument(name = "library.search_store.search", skip_all)]
    pub async fn search(
        &self,
        workspace_id: uuid::Uuid,
        query: &str,
        query_embedding: Option<Vec<f32>>,
        doc_type: Option<DocType>,
        limit: usize,
    ) -> Result<Vec<SearchResult>, LibraryError> {
        let ids = [workspace_id, uuid::Uuid::nil()];
        let doc_types: Vec<DocType> = doc_type.into_iter().collect();
        let hits = self
            .search_in_workspaces(Some(&ids), query, query_embedding, &doc_types, limit)
            .await?;
        Ok(hits
            .into_iter()
            .map(|h| SearchResult {
                doc_id: h.doc_id,
                doc_type: h.doc_type,
                title: h.title,
                content: h.content,
                tags: h.tags,
                score: h.score,
            })
            .collect())
    }

    /// Cross-workspace variant of [`Self::search`]. Empty `workspace_ids`
    /// = no filter (every workspace plus global). Otherwise the caller's
    /// list is used as-is, with the nil UUID appended so global-scoped
    /// files always surface alongside.
    #[tracing::instrument(name = "library.search_store.search_global", skip_all)]
    pub async fn search_global(
        &self,
        workspace_ids: &[uuid::Uuid],
        query: &str,
        query_embedding: Option<Vec<f32>>,
        doc_types: &[DocType],
        limit: usize,
    ) -> Result<Vec<GlobalSearchHit>, LibraryError> {
        let workspace_filter: Option<Vec<uuid::Uuid>> = if workspace_ids.is_empty() {
            None
        } else {
            let mut ids: Vec<uuid::Uuid> = workspace_ids.to_vec();
            if !ids.contains(&uuid::Uuid::nil()) {
                ids.push(uuid::Uuid::nil());
            }
            Some(ids)
        };
        self.search_in_workspaces(
            workspace_filter.as_deref(),
            query,
            query_embedding,
            doc_types,
            limit,
        )
        .await
    }

    /// Shared FTS + vector search. `workspace_filter = None` = no
    /// workspace clause; `Some(&[..])` filters to those ids.
    async fn search_in_workspaces(
        &self,
        workspace_filter: Option<&[uuid::Uuid]>,
        query: &str,
        query_embedding: Option<Vec<f32>>,
        doc_types: &[DocType],
        limit: usize,
    ) -> Result<Vec<GlobalSearchHit>, LibraryError> {
        let over_fetch = (limit * 3).max(10) as i64;
        let doc_type_filter: Option<Vec<String>> = if doc_types.is_empty() {
            None
        } else {
            Some(doc_types.iter().map(|d| d.as_str().to_string()).collect())
        };

        let fts_rows: Vec<GlobalFtsRow> = sqlx::query_as!(
            GlobalFtsRow,
            r#"SELECT doc_id, doc_type, workspace_id, title_text, content_text, tags,
                      ts_rank(search_tsv, plainto_tsquery('english', $1)) AS rank
               FROM library_search_data
               WHERE ($2::uuid[] IS NULL OR workspace_id = ANY($2))
                 AND search_tsv @@ plainto_tsquery('english', $1)
                 AND ($3::text[] IS NULL OR doc_type = ANY($3))
               ORDER BY rank DESC
               LIMIT $4"#,
            query,
            workspace_filter,
            doc_type_filter.as_deref(),
            over_fetch,
        )
        .fetch_all(&self.pool)
        .await?;

        let vec_rows: Vec<GlobalVecRow> = if let Some(emb) = query_embedding {
            let vec = Vector::from(emb);
            sqlx::query_as!(
                GlobalVecRow,
                r#"SELECT doc_id, doc_type, workspace_id, title_text, content_text, tags,
                          embedding <=> $1 AS distance
                   FROM library_search_data
                   WHERE ($2::uuid[] IS NULL OR workspace_id = ANY($2))
                     AND embedding IS NOT NULL
                     AND ($3::text[] IS NULL OR doc_type = ANY($3))
                   ORDER BY distance ASC
                   LIMIT $4"#,
                vec as Vector,
                workspace_filter,
                doc_type_filter.as_deref(),
                over_fetch,
            )
            .fetch_all(&self.pool)
            .await?
        } else {
            Vec::new()
        };

        let mut hits = rrf_fuse(fts_rows, vec_rows, limit);
        self.enrich_space_hits(&mut hits).await?;
        Ok(hits)
    }

    /// SpaceFile rows in `library_search_data` carry a nil `workspace_id`
    /// — the meaningful citation is `(space_slug, relative_path)`,
    /// which lives in `space_search_data` + `spaces`. Single batched
    /// query keyed by the SpaceFile doc_ids in the result set.
    async fn enrich_space_hits(
        &self,
        hits: &mut [GlobalSearchHit],
    ) -> Result<(), LibraryError> {
        let space_ids: Vec<uuid::Uuid> = hits
            .iter()
            .filter(|h| h.doc_type == DocType::SpaceFile)
            .map(|h| h.doc_id)
            .collect();
        if space_ids.is_empty() {
            return Ok(());
        }
        let rows = sqlx::query!(
            r#"SELECT ssd.doc_id, s.slug AS space_slug, ssd.relative_path
               FROM space_search_data ssd
               JOIN spaces s ON s.id = ssd.space_id
               WHERE ssd.doc_id = ANY($1)"#,
            &space_ids,
        )
        .fetch_all(&self.pool)
        .await?;
        let by_id: std::collections::HashMap<uuid::Uuid, (String, String)> = rows
            .into_iter()
            .map(|r| (r.doc_id, (r.space_slug, r.relative_path)))
            .collect();
        for hit in hits.iter_mut() {
            if hit.doc_type == DocType::SpaceFile {
                if let Some((slug, path)) = by_id.get(&hit.doc_id) {
                    hit.space_slug = Some(slug.clone());
                    hit.relative_path = Some(path.clone());
                }
            }
        }
        Ok(())
    }
}

struct GlobalFtsRow {
    doc_id: uuid::Uuid,
    doc_type: String,
    workspace_id: uuid::Uuid,
    title_text: String,
    content_text: String,
    tags: serde_json::Value,
    #[allow(dead_code)]
    rank: Option<f32>,
}

struct GlobalVecRow {
    doc_id: uuid::Uuid,
    doc_type: String,
    workspace_id: uuid::Uuid,
    title_text: String,
    content_text: String,
    tags: serde_json::Value,
    #[allow(dead_code)]
    distance: Option<f64>,
}

/// Reciprocal Rank Fusion of FTS + vector hits, normalized to `[0, 1]`.
/// Rank 0 in both lists → 1.0; rank 0 in one list → 0.5 (also the cap
/// when the embedder is unavailable and only FTS rows are present).
fn rrf_fuse(
    fts_rows: Vec<GlobalFtsRow>,
    vec_rows: Vec<GlobalVecRow>,
    limit: usize,
) -> Vec<GlobalSearchHit> {
    use std::collections::HashMap;

    const K: f64 = 60.0;
    const N_LISTS: f64 = 2.0;
    let max_score = N_LISTS / (K + 1.0);

    struct Candidate {
        doc_type: DocType,
        workspace_id: uuid::Uuid,
        title: String,
        content: String,
        tags: Vec<String>,
        score: f64,
    }

    let mut map: HashMap<(uuid::Uuid, String), Candidate> = HashMap::new();

    for (rank, row) in fts_rows.into_iter().enumerate() {
        let key = (row.doc_id, row.doc_type.clone());
        let entry = map.entry(key).or_insert_with(|| Candidate {
            doc_type: parse_doc_type(&row.doc_type),
            workspace_id: row.workspace_id,
            title: row.title_text.clone(),
            content: row.content_text.clone(),
            tags: parse_tags(&row.tags),
            score: 0.0,
        });
        entry.score += 1.0 / (K + rank as f64 + 1.0);
    }

    for (rank, row) in vec_rows.into_iter().enumerate() {
        let key = (row.doc_id, row.doc_type.clone());
        let entry = map.entry(key).or_insert_with(|| Candidate {
            doc_type: parse_doc_type(&row.doc_type),
            workspace_id: row.workspace_id,
            title: row.title_text.clone(),
            content: row.content_text.clone(),
            tags: parse_tags(&row.tags),
            score: 0.0,
        });
        entry.score += 1.0 / (K + rank as f64 + 1.0);
    }

    let mut results: Vec<GlobalSearchHit> = map
        .into_iter()
        .map(|((doc_id, _), c)| GlobalSearchHit {
            doc_id,
            doc_type: c.doc_type,
            workspace_id: c.workspace_id,
            title: c.title,
            content: c.content,
            tags: c.tags,
            score: (c.score / max_score).clamp(0.0, 1.0),
            space_slug: None,
            relative_path: None,
        })
        .collect();

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results.truncate(limit);
    results
}

fn parse_tags(val: &serde_json::Value) -> Vec<String> {
    val.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_doc_type(s: &str) -> DocType {
    match s {
        "note" => DocType::Note,
        "skill" => DocType::Skill,
        "workflow" => DocType::Workflow,
        "space_file" => DocType::SpaceFile,
        _ => DocType::Note,
    }
}
