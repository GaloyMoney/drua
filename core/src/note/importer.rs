use std::sync::Arc;

use drua_library::{
    DocType as DruaDocType, GitFileHash, LibraryImporter, SearchableFields, UpsertError,
};

use crate::library::AuthedSpaces;
use crate::note::file::parse_note_markdown;
use crate::note::{ImportScope, Notes};
use crate::project::Projects;

/// Adapter that lets the drua_library reverse-sync job route note
/// markdown paths into the Notes service. Two layouts:
///
/// - `runtime/projects/{project}/notes/*.md` — project-scoped
/// - `spaces/{slug}/notes/*.md` — space-scoped (the spaces tree
///   intentionally lives at the repo root, not under `runtime/`; see
///   `library::Spaces::write_file`)
///
/// Notes have no global tier — every note belongs to a project or a
/// space (CHECK constraint on the `notes` table).
pub struct NotesImporter {
    notes: Arc<Notes>,
    projects: Arc<Projects>,
    spaces: AuthedSpaces,
}

impl NotesImporter {
    pub fn new(notes: Arc<Notes>, projects: Arc<Projects>, spaces: AuthedSpaces) -> Self {
        Self {
            notes,
            projects,
            spaces,
        }
    }
}

#[async_trait::async_trait]
impl LibraryImporter for NotesImporter {
    fn matches(&self, path: &str) -> bool {
        if !path.ends_with(".md") {
            return false;
        }
        let parts: Vec<&str> = path.split('/').collect();
        matches!(
            parts.as_slice(),
            ["runtime", "projects", _, "notes", _] | ["spaces", _, "notes", _]
        )
    }

    fn doc_type(&self) -> DruaDocType {
        DruaDocType::new("note")
    }

    async fn upsert_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        _old_file_hash: Option<GitFileHash>,
        _file_hash: GitFileHash,
        path: &str,
        content: &[u8],
    ) -> Result<Option<SearchableFields>, UpsertError> {
        let content_str = std::str::from_utf8(content)
            .map_err(|e| UpsertError::Parse(format!("non-utf8 note content: {e}")))?;
        let Some(parsed) = parse_note_markdown(content_str, path) else {
            return Ok(None);
        };

        let scope = if let Some(slug) = parsed.space_slug.as_deref() {
            match self.spaces.find_by_slug(slug).await {
                Ok(Some(space)) => ImportScope::Space {
                    space_id: space.id,
                    space_slug: space.slug,
                },
                Ok(None) => {
                    tracing::debug!(
                        space_slug = %slug,
                        path,
                        "note references unknown space; skipping import"
                    );
                    return Ok(None);
                }
                Err(e) => {
                    return Err(UpsertError::Other(format!(
                        "space lookup failed for {slug}: {e}"
                    )))
                }
            }
        } else if let Some(name) = parsed.project_name.as_deref() {
            match self.projects.find_by_name(name).await {
                Ok(Some(project)) => ImportScope::Project {
                    project_id: project.id,
                    project_name: project.name.clone(),
                },
                Ok(None) => {
                    tracing::debug!(
                        project_name = %name,
                        path,
                        "note references unknown project; skipping import"
                    );
                    return Ok(None);
                }
                Err(e) => {
                    return Err(UpsertError::Other(format!(
                        "project lookup failed for {name}: {e}"
                    )))
                }
            }
        } else {
            // Should never happen — `matches()` only accepts paths whose
            // shape forces exactly one of project_name/space_slug to be set.
            tracing::warn!(
                path,
                "note path matched importer but yielded no scope; skipping"
            );
            return Ok(None);
        };

        // Always returns Ok(None): the entity's post_persist_hook owns
        // search+embed (mirrors SkillsImporter rationale).
        self.notes
            .import_from_library(op, parsed, scope)
            .await
            .map_err(|e| UpsertError::Other(format!("note upsert: {e}")))?;

        Ok(None)
    }

    /// Reverse-sync delete: an external author removed the note's
    /// markdown file in git. Soft-delete the matching DB row by
    /// `library_documents.path` lookup (avoids id-prefix collision
    /// risk; the search-store row is rewritten in lockstep with the
    /// entity's path).
    async fn delete_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        path: &str,
        _content: &[u8],
    ) -> Result<Option<uuid::Uuid>, UpsertError> {
        let deleted = self
            .notes
            .delete_by_path_in_op(op, path)
            .await
            .map_err(|e| UpsertError::Other(format!("note reverse-delete: {e}")))?;
        Ok(deleted.map(uuid::Uuid::from))
    }
}
