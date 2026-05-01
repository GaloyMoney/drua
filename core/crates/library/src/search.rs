use sqlx::PgPool;

use crate::LibraryError;

#[derive(Debug, Clone)]
pub struct SearchableFields {
    pub doc_id: uuid::Uuid,
    pub doc_type: String,
    pub scope_id: Option<uuid::Uuid>,
    pub scope_slug: Option<String>,
    pub name: String,
    pub path: Option<String>,
    pub content: String,
}

#[derive(Clone)]
pub struct SearchStore {
    #[allow(dead_code)]
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
            fields.doc_type,
            fields.scope_id,
            fields.scope_slug,
            fields.name,
            fields.path,
            fields.content,
        )
        .execute(op.as_executor())
        .await?;
        Ok(())
    }
}
