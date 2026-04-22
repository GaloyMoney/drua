mod entity;
pub mod error;
pub(crate) mod repo;
mod store;

use std::sync::Arc;

use tracing::instrument;

pub use entity::Note;
use entity::*;
pub use error::*;
use repo::*;
pub use store::NoteSearchResult;
use store::*;

use crate::library::Library;
use crate::primitives::*;

#[derive(Clone)]
pub struct Notes {
    repo: NoteRepo,
    search: NoteSearchStore,
    library: Library,
    embedder: Arc<code_assistant_core::embedder::Embedder>,
}

impl Notes {
    pub fn new(
        pool: &sqlx::PgPool,
        library: Library,
        embedder: Arc<code_assistant_core::embedder::Embedder>,
    ) -> Self {
        Self {
            repo: NoteRepo::new(pool),
            search: NoteSearchStore::new(pool),
            library,
            embedder,
        }
    }

    /// Create a new note in a workspace.
    #[instrument(name = "note.store", skip(self))]
    pub async fn store(
        &self,
        workspace_id: WorkspaceId,
        workspace_name: &str,
        title: String,
        content: String,
        tags: Vec<String>,
    ) -> Result<Note, NoteError> {
        let new_note = NewNote::builder()
            .workspace_id(workspace_id)
            .title(&title)
            .content(&content)
            .tags(tags)
            .build()
            .expect("NewNote builder should not fail");

        let note = self.repo.create(new_note).await?;

        self.search.upsert(&note).await?;
        self.spawn_embed(note.id, &note.title, &note.content);
        self.write_to_library(&note, workspace_name).await;

        Ok(note)
    }

    /// Update an existing note.
    #[instrument(name = "note.update", skip(self))]
    pub async fn update(
        &self,
        workspace_id: WorkspaceId,
        workspace_name: &str,
        note_id: NoteId,
        title: String,
        content: String,
        tags: Vec<String>,
    ) -> Result<Note, NoteError> {
        let mut note = self.repo.find_by_id(note_id).await?;
        if note.workspace_id != workspace_id {
            return Err(NoteError::Authorization(
                crate::primitives::AuthorizationError::Forbidden {
                    verb: crate::primitives::AuthVerb::Read,
                    resource: crate::primitives::AuthResource::Workspace(Some(workspace_id)),
                },
            ));
        }

        note.update(title, content, tags);
        self.repo.update(&mut note).await?;

        self.search.upsert(&note).await?;
        self.spawn_embed(note.id, &note.title, &note.content);
        self.write_to_library(&note, workspace_name).await;

        Ok(note)
    }

    /// Create or update a note. If `note_id` is provided, update; otherwise create.
    #[instrument(name = "note.store_or_update", skip(self))]
    pub async fn store_or_update(
        &self,
        workspace_id: WorkspaceId,
        workspace_name: &str,
        note_id: Option<NoteId>,
        title: String,
        content: String,
        tags: Vec<String>,
    ) -> Result<Note, NoteError> {
        match note_id {
            Some(id) => {
                self.update(workspace_id, workspace_name, id, title, content, tags)
                    .await
            }
            None => {
                self.store(workspace_id, workspace_name, title, content, tags)
                    .await
            }
        }
    }

    /// Retrieve a single note by id, scoped to workspace.
    #[instrument(name = "note.find_by_id", skip(self))]
    pub async fn find_by_id(
        &self,
        workspace_id: WorkspaceId,
        note_id: NoteId,
    ) -> Result<Note, NoteError> {
        let note = self.repo.find_by_id(note_id).await?;
        if note.workspace_id != workspace_id {
            return Err(NoteError::Authorization(
                crate::primitives::AuthorizationError::Forbidden {
                    verb: crate::primitives::AuthVerb::Read,
                    resource: crate::primitives::AuthResource::Workspace(Some(workspace_id)),
                },
            ));
        }
        Ok(note)
    }

    /// Hybrid search across workspace notes.
    #[instrument(name = "note.search", skip(self))]
    pub async fn search(
        &self,
        workspace_id: WorkspaceId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<NoteSearchResult>, NoteError> {
        let query_embedding = match self.embedder.embed_query(query).await {
            Ok(emb) => Some(emb),
            Err(e) => {
                tracing::warn!(error = %e, "embedding query failed, falling back to FTS-only");
                None
            }
        };
        self.search
            .search(workspace_id, query, query_embedding, limit)
            .await
    }

    /// List all notes in a workspace.
    #[instrument(name = "note.list", skip(self))]
    pub async fn list(
        &self,
        workspace_id: WorkspaceId,
        limit: usize,
    ) -> Result<Vec<NoteSearchResult>, NoteError> {
        self.search.list(workspace_id, limit).await
    }

    // -- internal helpers ---------------------------------------------------

    fn spawn_embed(&self, note_id: NoteId, title: &str, content: &str) {
        let embedder = self.embedder.clone();
        let search = self.search.clone();
        let text = format!("{}\n\n{}", title, content);
        tokio::spawn(async move {
            match embedder.embed_document(&text).await {
                Ok(embedding) => {
                    if let Err(e) = search.set_embedding(note_id, embedding).await {
                        tracing::error!(note_id = %note_id, error = %e, "failed to store embedding");
                    }
                }
                Err(e) => {
                    tracing::error!(note_id = %note_id, error = %e, "failed to generate embedding");
                }
            }
        });
    }

    async fn write_to_library(&self, note: &Note, workspace_name: &str) {
        let slug = slugify(&note.title);
        let short_id = &note.id.to_string()[..8];
        let relative_path = format!(
            "runtime/workspaces/{}/notes/{}-{}.md",
            workspace_name, slug, short_id
        );

        let created_at = note
            .events
            .entity_first_persisted_at()
            .map(|t| t.to_rfc3339())
            .unwrap_or_default();
        let tags_str = note
            .tags
            .iter()
            .map(|t| format!("\"{}\"", t))
            .collect::<Vec<_>>()
            .join(", ");

        let markdown = format!(
            "---\nid: {}\nworkspace: {}\ntags: [{}]\ncreated: {}\n---\n\n# {}\n\n{}\n",
            note.id, note.workspace_id, tags_str, created_at, note.title, note.content
        );

        let commit_msg = format!("note: {}", note.title);
        if let Err(e) = self.library.write_runtime_file(&relative_path, &markdown, &commit_msg).await {
            tracing::error!(
                note_id = %note.id,
                path = %relative_path,
                error = %e,
                "failed to write note to library"
            );
        }
    }
}

fn slugify(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::slugify;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("  Spaces & Symbols!!  "), "spaces-symbols");
        assert_eq!(slugify("already-slugged"), "already-slugged");
    }
}
