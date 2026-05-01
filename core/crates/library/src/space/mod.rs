mod entity;
pub mod error;
pub(crate) mod repo;

use std::sync::Arc;

pub use entity::{NewSpace, Space, SpaceEvent};
pub use error::*;

use self::repo::SpaceRepo;
use crate::git::GitEngine;
use crate::importer::{DocType, GitFileHash, LibraryImporter, UpsertError};
use crate::SearchableFields;

const SPACE_DOC_TYPE: DocType = DocType::new("space_file");

/// UUID v5 namespace for deriving deterministic doc_ids from `<slug>/<rel_path>`.
const SPACE_DOC_NAMESPACE: uuid::Uuid = uuid::Uuid::from_bytes([
    0x4e, 0x97, 0x05, 0x53, 0x53, 0x46, 0x4d, 0x73, 0xa3, 0x9d, 0xa6, 0x4d, 0x6e, 0x39, 0x4d, 0x53,
]);

#[derive(Clone)]
pub struct Spaces {
    git: Arc<GitEngine>,
    repo: SpaceRepo,
}

impl Spaces {
    pub fn new(git: &Arc<GitEngine>, pool: &sqlx::PgPool) -> Self {
        Self {
            git: Arc::clone(git),
            repo: SpaceRepo::new(pool),
        }
    }

    #[tracing::instrument(name = "library.spaces.create", skip_all, fields(%slug))]
    pub async fn create(
        &self,
        slug: String,
        description: Option<String>,
    ) -> Result<Space, SpaceError> {
        let mut op = self.repo.begin_op().await?;
        let space = self.create_in_op(&mut op, slug, description).await?;
        op.commit().await?;
        Ok(space)
    }

    #[tracing::instrument(name = "library.spaces.create_in_op", skip_all, fields(%slug))]
    pub async fn create_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        slug: String,
        description: Option<String>,
    ) -> Result<Space, SpaceError> {
        let mut builder = NewSpace::builder().slug(slug);
        if let Some(desc) = description {
            builder = builder.description(desc);
        }
        let new_space = builder.build()?;
        let space = self.repo.create_in_op(op, new_space).await?;

        self.git
            .update_file(
                format!("spaces/{}/.gitkeep", space.slug),
                |_input_path, _current| (None, Some(Vec::new())),
                format!("space: init {}", space.slug),
            )
            .await
            .map_err(|e| SpaceError::Git(e.to_string()))?;

        Ok(space)
    }
}

#[async_trait::async_trait]
impl LibraryImporter for Spaces {
    fn matches(&self, path: &str) -> bool {
        let mut parts = path.splitn(3, '/');
        parts.next() == Some("spaces") && parts.next().is_some() && parts.next().is_some()
    }

    fn doc_type(&self) -> DocType {
        SPACE_DOC_TYPE
    }

    async fn upsert_in_op(
        &self,
        _op: &mut es_entity::DbOp<'_>,
        _old_file_hash: Option<GitFileHash>,
        _file_hash: GitFileHash,
        path: &str,
        content: &[u8],
    ) -> Result<Option<SearchableFields>, UpsertError> {
        if path.ends_with("/.gitkeep") {
            return Ok(None);
        }
        let mut parts = path.splitn(3, '/');
        let _ = parts.next();
        let slug = parts
            .next()
            .ok_or_else(|| UpsertError::Parse(format!("bad space path: {path}")))?;
        let rel = parts
            .next()
            .ok_or_else(|| UpsertError::Parse(format!("bad space path: {path}")))?;

        let space = match self
            .repo
            .maybe_find_by_slug(slug)
            .await
            .map_err(|e| UpsertError::Other(e.to_string()))?
        {
            Some(s) => s,
            None => {
                tracing::debug!(slug, path, "space not found; skipping");
                return Ok(None);
            }
        };

        let content_str = std::str::from_utf8(content)
            .map_err(|e| UpsertError::Parse(format!("non-utf8 content: {e}")))?
            .to_string();
        let name = std::path::Path::new(rel)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(rel)
            .to_string();

        let doc_id = uuid::Uuid::new_v5(
            &SPACE_DOC_NAMESPACE,
            format!("{}/{}", space.slug, rel).as_bytes(),
        );

        Ok(Some(SearchableFields {
            doc_id,
            doc_type: SPACE_DOC_TYPE,
            scope_id: Some(space.id.into()),
            scope_slug: Some(space.slug.clone()),
            name,
            path: Some(rel.to_string()),
            content: content_str,
        }))
    }
}
