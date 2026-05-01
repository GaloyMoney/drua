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
                |_input_path, _current| Ok((None, Some(Vec::new()))),
                format!("space: init {}", space.slug),
            )
            .await
            .map_err(|e| SpaceError::Git(e.to_string()))?;

        Ok(space)
    }

    /// Blind overwrite of `spaces/{slug}/{relative_path}`.
    #[tracing::instrument(name = "library.spaces.write_file", skip_all, fields(%slug, %relative_path))]
    pub async fn write_file(
        &self,
        slug: &str,
        relative_path: &str,
        content: String,
    ) -> Result<(), SpaceError> {
        let path = format!("spaces/{slug}/{relative_path}");
        let bytes = content.into_bytes();
        self.git
            .update_file(
                path,
                move |_, _| Ok((None, Some(bytes.clone()))),
                format!("space:{slug}: write {relative_path}"),
            )
            .await
            .map_err(|e| SpaceError::Git(e.to_string()))
    }

    /// Removes `spaces/{slug}/{relative_path}`. No-op if absent at HEAD.
    #[tracing::instrument(name = "library.spaces.delete_file", skip_all, fields(%slug, %relative_path))]
    pub async fn delete_file(&self, slug: &str, relative_path: &str) -> Result<(), SpaceError> {
        let path = format!("spaces/{slug}/{relative_path}");
        self.git
            .update_file(
                path,
                |_, _| Ok((None, None)),
                format!("space:{slug}: delete {relative_path}"),
            )
            .await
            .map_err(|e| SpaceError::Git(e.to_string()))
    }

    /// Read–modify–write substitution: errors if `old_str` doesn't appear
    /// exactly once in the freshest disk content.
    #[tracing::instrument(name = "library.spaces.str_replace", skip_all, fields(%slug, %relative_path))]
    pub async fn str_replace(
        &self,
        slug: &str,
        relative_path: &str,
        old_str: String,
        new_str: String,
    ) -> Result<(), SpaceError> {
        let path = format!("spaces/{slug}/{relative_path}");
        let path_for_err = path.clone();
        self.git
            .update_file(
                path,
                move |_, current| {
                    let current = current.ok_or_else(|| {
                        crate::LibraryError::Validation(format!(
                            "str_replace: file does not exist: {path_for_err}"
                        ))
                    })?;
                    let current_str = std::str::from_utf8(current).map_err(|e| {
                        crate::LibraryError::Validation(format!(
                            "str_replace: non-utf8 content in {path_for_err}: {e}"
                        ))
                    })?;
                    let count = current_str.matches(&old_str).count();
                    if count == 0 {
                        return Err(crate::LibraryError::Validation(format!(
                            "str_replace: old_str not found in {path_for_err}"
                        )));
                    }
                    if count > 1 {
                        return Err(crate::LibraryError::Validation(format!(
                            "str_replace: old_str appears {count} times in {path_for_err}; must be unique"
                        )));
                    }
                    let new_content = current_str.replacen(&old_str, &new_str, 1);
                    Ok((None, Some(new_content.into_bytes())))
                },
                format!("space:{slug}: edit {relative_path}"),
            )
            .await
            .map_err(|e| match e {
                crate::LibraryError::Validation(msg) => SpaceError::Validation(msg),
                other => SpaceError::Git(other.to_string()),
            })
    }

    /// Read–modify–write insert. `line_number == 0` inserts at the
    /// beginning; out-of-range numbers append at EOF.
    #[tracing::instrument(name = "library.spaces.insert", skip_all, fields(%slug, %relative_path))]
    pub async fn insert(
        &self,
        slug: &str,
        relative_path: &str,
        line_number: usize,
        text: String,
    ) -> Result<(), SpaceError> {
        let path = format!("spaces/{slug}/{relative_path}");
        let path_for_err = path.clone();
        self.git
            .update_file(
                path,
                move |_, current| {
                    let current = current.ok_or_else(|| {
                        crate::LibraryError::Validation(format!(
                            "insert: file does not exist: {path_for_err}"
                        ))
                    })?;
                    let current_str = std::str::from_utf8(current).map_err(|e| {
                        crate::LibraryError::Validation(format!(
                            "insert: non-utf8 content in {path_for_err}: {e}"
                        ))
                    })?;
                    let mut lines: Vec<String> = current_str.lines().map(String::from).collect();
                    let idx = line_number.min(lines.len());
                    for (offset, t) in text.lines().enumerate() {
                        lines.insert(idx + offset, t.to_string());
                    }
                    let mut new_content = lines.join("\n");
                    if current_str.ends_with('\n') {
                        new_content.push('\n');
                    }
                    Ok((None, Some(new_content.into_bytes())))
                },
                format!("space:{slug}: insert {relative_path}"),
            )
            .await
            .map_err(|e| match e {
                crate::LibraryError::Validation(msg) => SpaceError::Validation(msg),
                other => SpaceError::Git(other.to_string()),
            })
    }

    /// Renames `spaces/{slug}/{from}` → `spaces/{slug}/{to}`. Errors if
    /// `from` is missing or `to` already exists.
    #[tracing::instrument(name = "library.spaces.move_file", skip_all, fields(%slug, %from, %to))]
    pub async fn move_file(&self, slug: &str, from: &str, to: &str) -> Result<(), SpaceError> {
        let from_path = format!("spaces/{slug}/{from}");
        let to_path = format!("spaces/{slug}/{to}");
        self.git
            .move_file(
                from_path,
                to_path,
                format!("space:{slug}: move {from} -> {to}"),
            )
            .await
            .map_err(|e| match e {
                crate::LibraryError::Validation(msg) => SpaceError::Validation(msg),
                other => SpaceError::Git(other.to_string()),
            })
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

    async fn delete_in_op(
        &self,
        _op: &mut es_entity::DbOp<'_>,
        path: &str,
    ) -> Result<Option<uuid::Uuid>, UpsertError> {
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

        Ok(Some(uuid::Uuid::new_v5(
            &SPACE_DOC_NAMESPACE,
            format!("{slug}/{rel}").as_bytes(),
        )))
    }
}
