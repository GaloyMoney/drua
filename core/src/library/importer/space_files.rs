use std::sync::Arc;

use crate::library::space::file_sync::{doc_id_for, extract_title_and_body};
use crate::library::{
    DocType, GitFileHash, Library, LibraryImporter, ParsedFile, SearchableFields, SyncedFile,
    UpsertError,
};
use crate::primitives::ProjectId;

/// Reverse-syncs `spaces/<slug>/**/*.md` into `space_search_data`.
/// Not entity-backed; `doc_id` is `uuidv5(SPACE_FILE_NAMESPACE, "{space_id}:{rel_path}")`.
///
/// `parse` stuffs the slug into `SyncedFile::project_name` (resolution
/// token; the unified runner treats it as a slug rather than a project
/// name when `doc_type == SpaceFile`) and the relative path into
/// `SyncedFile::slug`.
pub struct SpaceFilesImporter {
    library: Arc<Library>,
}

impl SpaceFilesImporter {
    pub fn new(library: Arc<Library>) -> Self {
        Self { library }
    }
}

fn split_space_path(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("spaces/")?;
    let (slug, rel_path) = rest.split_once('/')?;
    if slug.is_empty() || rel_path.is_empty() {
        return None;
    }
    Some((slug, rel_path))
}

impl LibraryImporter for SpaceFilesImporter {
    fn matches(&self, path: &str) -> bool {
        path.ends_with(".md") && split_space_path(path).is_some()
    }

    fn doc_type(&self) -> DocType {
        DocType::SpaceFile
    }

    fn parse(&self, content: &[u8], path: &str) -> Option<ParsedFile> {
        let content = std::str::from_utf8(content).ok()?;
        let (slug, rel_path) = split_space_path(path)?;
        let (title, body) = extract_title_and_body(content, rel_path);

        Some(ParsedFile {
            file: SyncedFile {
                doc_id: uuid::Uuid::nil(),
                doc_type: DocType::SpaceFile,
                project_id: None,
                // Resolution token: unified runner reads slug from here
                // when doc_type == SpaceFile (no project lookup happens).
                project_name: Some(slug.to_string()),
                slug: rel_path.to_string(),
                id_prefix: String::new(),
                created_at: String::new(),
                updated_at: String::new(),
                title,
                body: body.clone(),
                tags: Vec::new(),
                original_path: Some(path.to_string()),
                rendered: body,
            },
            needs_rewrite: false,
        })
    }

    async fn upsert_in_op(
        &self,
        _op: &mut es_entity::DbOp<'_>,
        file: &SyncedFile,
        _path: &str,
        _project: Option<ProjectId>,
        hash: GitFileHash,
    ) -> Result<(), UpsertError> {
        let slug = file
            .project_name
            .as_deref()
            .ok_or("missing slug resolution token")?;
        let rel_path = &file.slug;

        let Some(space) = self.library.find_space_by_slug(slug).await? else {
            tracing::warn!(%slug, %rel_path, "no space matches slug, skipping");
            return Ok(());
        };
        let doc_id = doc_id_for(space.id, rel_path);

        let fields = SearchableFields {
            doc_id,
            doc_type: DocType::SpaceFile,
            project_id: uuid::Uuid::nil(),
            title: file.title.clone(),
            body: file.body.clone(),
            tags: Vec::new(),
        };

        let upserted = self
            .library
            .search_store()
            .upsert_space_file_if_changed(&fields, space.id, rel_path, &hash)
            .await?;
        if !upserted {
            tracing::debug!(%slug, %rel_path, "space file unchanged, skipping");
            return Ok(());
        }

        // Embedding is best-effort; FTS works without it.
        let text = fields.text_for_embedding();
        match self.library.embedder().embed_document(&text).await {
            Ok(emb) => {
                if let Err(e) = self
                    .library
                    .search_store()
                    .set_embedding(doc_id, DocType::SpaceFile, emb)
                    .await
                {
                    tracing::warn!(error = %e, %slug, %rel_path, "set_embedding failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, %slug, %rel_path, "embed_document failed"),
        }
        Ok(())
    }

    async fn delete_in_op(
        &self,
        _op: &mut es_entity::DbOp<'_>,
        path: &str,
    ) -> Result<(), UpsertError> {
        let Some((slug, rel_path)) = split_space_path(path) else {
            return Ok(());
        };
        let Some(space) = self.library.find_space_by_slug(slug).await? else {
            return Ok(());
        };
        let doc_id = doc_id_for(space.id, rel_path);
        self.library
            .search_store()
            .delete_space_file(doc_id)
            .await?;
        Ok(())
    }
}
