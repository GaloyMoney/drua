use sqlx::PgPool;

use crate::importer::DocType;
use crate::LibraryError;

#[derive(Debug, Clone)]
pub struct SearchableFields {
    pub doc_id: uuid::Uuid,
    pub doc_type: DocType,
    pub scope_id: Option<uuid::Uuid>,
    pub scope_slug: Option<String>,
    pub name: String,
    pub path: Option<String>,
    pub content: String,
}

#[derive(Clone)]
pub struct SearchStore {
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
    ) -> Result<(), LibraryError> {
        sqlx::query!(
            r#"INSERT INTO library_documents
                   (doc_id, doc_type, scope_id, scope_slug, name, path, content)
               VALUES ($1, $2, $3, $4, $5, $6, $7)
               ON CONFLICT (doc_id, doc_type) DO UPDATE SET
                   scope_id   = EXCLUDED.scope_id,
                   scope_slug = EXCLUDED.scope_slug,
                   name       = EXCLUDED.name,
                   path       = EXCLUDED.path,
                   content    = EXCLUDED.content"#,
            fields.doc_id,
            fields.doc_type.as_str(),
            fields.scope_id,
            fields.scope_slug,
            fields.name,
            fields.path,
            fields.content,
        )
        .execute(op.as_executor())
        .await?;
        // @@ spawn a job that executes the embedding
        Ok(())
    }

    /// Plain FTS over `library_documents.search_tsv`. Embedding-based fusion
    /// (`@@ blend FTS + vector`) and scope filtering are deferred — the
    /// signature already accepts `query_embedding` and `doc_type` so callers
    /// can be kept stable when those are wired in.
    #[tracing::instrument(name = "library.search_store.search", skip_all)]
    pub async fn search(
        &self,
        query: &str,
        _query_embedding: Option<Vec<f32>>,
        doc_type: Option<DocType>,
        limit: usize,
    ) -> Result<Vec<SearchableFields>, LibraryError> {
        let doc_type_filter: Option<&str> = doc_type.as_ref().map(|d| d.as_str());
        let rows = sqlx::query!(
            r#"SELECT doc_id, doc_type, scope_id, scope_slug, name, path, content
               FROM library_documents
               WHERE search_tsv @@ plainto_tsquery('english', $1)
                 AND ($2::text IS NULL OR doc_type = $2)
               ORDER BY ts_rank(search_tsv, plainto_tsquery('english', $1)) DESC
               LIMIT $3"#,
            query,
            doc_type_filter,
            limit as i64,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| SearchableFields {
                doc_id: r.doc_id,
                doc_type: DocType::from_owned(r.doc_type),
                scope_id: r.scope_id,
                scope_slug: r.scope_slug,
                name: r.name,
                path: r.path,
                content: r.content,
            })
            .collect())
    }
}
