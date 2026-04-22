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

use crate::auth::AuthSubject;
use crate::library::{Library, RuntimeFile};
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
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
        workspace_name: &str,
        title: String,
        content: String,
        tags: Vec<String>,
    ) -> Result<Note, NoteError> {
        sub.can(AuthVerb::Create, AuthResource::Note(workspace_id, None))?;

        let note_id = NoteId::new();
        let runtime_file = RuntimeFile::for_note(
            note_id,
            workspace_id,
            workspace_name,
            &title,
            &content,
            &tags,
            "",
        );
        let file_hash = runtime_file.file_hash();

        let new_note = NewNote::builder()
            .id(note_id)
            .workspace_id(workspace_id)
            .title(&title)
            .content(&content)
            .tags(tags)
            .file_hash(file_hash)
            .build()
            .expect("NewNote builder should not fail");

        let mut op = self.repo.begin_op().await?;
        let note = self.repo.create_in_op(&mut op, new_note).await?;
        self.search.upsert_in_op(&mut op, &note).await?;
        self.library.write(runtime_file).await?;
        op.commit().await?;

        self.spawn_embed(note.id, &note.title, &note.content);

        Ok(note)
    }

    /// Update an existing note.
    #[allow(clippy::too_many_arguments)]
    #[instrument(name = "note.update", skip(self))]
    pub async fn update(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
        workspace_name: &str,
        note_id: NoteId,
        title: String,
        content: String,
        tags: Vec<String>,
    ) -> Result<Note, NoteError> {
        let mut note = self.repo.find_by_id(note_id).await?;
        if note.workspace_id != workspace_id {
            return Err(NoteError::Authorization(AuthorizationError::Forbidden {
                verb: AuthVerb::Read,
                resource: AuthResource::Workspace(Some(workspace_id)),
            }));
        }
        sub.can(
            AuthVerb::Update,
            AuthResource::Note(workspace_id, Some(note_id)),
        )?;

        let created_at = note
            .events
            .entity_first_persisted_at()
            .map(|t| t.to_rfc3339())
            .unwrap_or_default();
        let runtime_file = RuntimeFile::for_note(
            note.id,
            note.workspace_id,
            workspace_name,
            &title,
            &content,
            &tags,
            &created_at,
        );
        let file_hash = runtime_file.file_hash();

        note.update(title, content, tags, file_hash);

        let mut op = self.repo.begin_op().await?;
        self.repo.update_in_op(&mut op, &mut note).await?;
        self.search.upsert_in_op(&mut op, &note).await?;
        self.library.write(runtime_file).await?;
        op.commit().await?;

        self.spawn_embed(note.id, &note.title, &note.content);

        Ok(note)
    }

    /// Create or update a note. If `note_id` is provided, update; otherwise create.
    #[allow(clippy::too_many_arguments)]
    #[instrument(name = "note.store_or_update", skip(self))]
    pub async fn store_or_update(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
        workspace_name: &str,
        note_id: Option<NoteId>,
        title: String,
        content: String,
        tags: Vec<String>,
    ) -> Result<Note, NoteError> {
        match note_id {
            Some(id) => {
                self.update(sub, workspace_id, workspace_name, id, title, content, tags)
                    .await
            }
            None => {
                self.store(sub, workspace_id, workspace_name, title, content, tags)
                    .await
            }
        }
    }

    /// Retrieve a single note by id, scoped to workspace.
    #[instrument(name = "note.find_by_id", skip(self))]
    pub async fn find_by_id(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
        note_id: NoteId,
    ) -> Result<Note, NoteError> {
        sub.can(
            AuthVerb::Read,
            AuthResource::Note(workspace_id, Some(note_id)),
        )?;
        let note = self.repo.find_by_id(note_id).await?;
        if note.workspace_id != workspace_id {
            return Err(NoteError::Authorization(AuthorizationError::Forbidden {
                verb: AuthVerb::Read,
                resource: AuthResource::Workspace(Some(workspace_id)),
            }));
        }
        Ok(note)
    }

    /// Hybrid search across workspace notes.
    #[instrument(name = "note.search", skip(self))]
    pub async fn search(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<NoteSearchResult>, NoteError> {
        sub.can(AuthVerb::Read, AuthResource::Note(workspace_id, None))?;
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
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
        limit: usize,
    ) -> Result<Vec<NoteSearchResult>, NoteError> {
        sub.can(AuthVerb::Read, AuthResource::Note(workspace_id, None))?;
        let query = es_entity::PaginatedQueryArgs {
            first: limit,
            after: None,
        };
        let result = self
            .repo
            .list_for_workspace_id_by_created_at(
                workspace_id,
                query,
                es_entity::ListDirection::Descending,
            )
            .await?;
        Ok(result
            .entities
            .into_iter()
            .map(|n| NoteSearchResult {
                id: n.id,
                title: n.title,
                content: n.content,
                tags: n.tags,
                score: 0.0,
            })
            .collect())
    }

    // -- internal helpers ---------------------------------------------------

    fn spawn_embed(&self, note_id: NoteId, title: &str, content: &str) {
        let embedder = self.embedder.clone();
        let search = self.search.clone();
        let text = format!("{}\n\n{}", title, content);
        // Fire-and-forget: embed_document uses spawn_blocking internally (ONNX
        // inference is CPU-heavy). The outer tokio::spawn lets us return the
        // note to the caller without waiting for embedding completion.
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

}
