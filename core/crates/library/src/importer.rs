use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::SearchableFields;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[sqlx(transparent)]
#[serde(transparent)]
pub struct DocType(Cow<'static, str>);

impl DocType {
    pub const fn new(doc_type: &'static str) -> Self {
        DocType(Cow::Borrowed(doc_type))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for DocType {
    fn from(s: String) -> Self {
        DocType(Cow::Owned(s))
    }
}

impl std::fmt::Display for DocType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GitFileHash(pub String);

impl GitFileHash {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for GitFileHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UpsertError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("parse: {0}")]
    Parse(String),
    #[error("other: {0}")]
    Other(String),
}

#[async_trait::async_trait]
pub trait LibraryImporter: Send + Sync + 'static {
    /// True if this importer claims the path.
    fn matches(&self, path: &str) -> bool;

    fn doc_type(&self) -> DocType;

    /// Is the document with this id still live (present and not soft-deleted)?
    /// Consulted by the forward-sync write job to skip a stale, replayed write
    /// that would resurrect a deleted entity's file. Keyed by doc id (not
    /// path) to stay robust under path aliasing. Default: always live (no
    /// liveness gating).
    async fn is_doc_live(&self, _id: uuid::Uuid) -> Result<bool, UpsertError> {
        Ok(true)
    }

    /// Upsert the document into its service-specific tables. Returns
    /// `Some(SearchableFields)` when the caller should also write the
    /// search index row; `None` when the importer handled everything
    /// itself (or skipped — e.g. content unchanged).
    async fn upsert_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        old_file_hash: Option<GitFileHash>,
        file_hash: GitFileHash,
        path: &str,
        content: &[u8],
    ) -> Result<Option<SearchableFields>, UpsertError>;

    /// Reverse of [`Self::upsert_in_op`]. Returns `Some(doc_id)` to signal
    /// the runner should also delete the search-store row (paired with
    /// [`Self::doc_type`]); `None` skips. Default: warn-and-skip.
    ///
    /// `content` is the removed file's old blob bytes (empty if unreadable),
    /// so importers can make id-aware delete decisions — only delete the
    /// entity whose file this actually was. This backstops the projection
    /// trailer: a historical (pre-trailer) or external removal still can't
    /// soft-delete a different generation that now owns the same path.
    async fn delete_in_op(
        &self,
        _op: &mut es_entity::DbOp<'_>,
        path: &str,
        _content: &[u8],
    ) -> Result<Option<uuid::Uuid>, UpsertError> {
        tracing::warn!(%path, doc_type = self.doc_type().as_str(), "delete_in_op not implemented; skipping");
        Ok(None)
    }
}
