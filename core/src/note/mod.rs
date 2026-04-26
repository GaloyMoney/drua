mod entity;
pub mod error;
pub(crate) mod repo;

use tracing::instrument;

pub use entity::Note;
use entity::*;
pub use error::*;
use repo::*;

use crate::auth::AuthSubject;
use crate::library::{DocType, Library, RuntimeFile, SearchResult};
use crate::primitives::*;

#[derive(Clone)]
pub struct Notes {
    repo: NoteRepo,
    library: Library,
    pool: sqlx::PgPool,
    context_generation: ContextGeneration,
}

impl Notes {
    pub fn new(
        pool: &sqlx::PgPool,
        library: Library,
        context_generation: ContextGeneration,
    ) -> Self {
        Self {
            repo: NoteRepo::new(pool, library.clone()),
            library,
            pool: pool.clone(),
            context_generation,
        }
    }

    /// Bump the local context generation counter and broadcast to other
    /// instances via PG NOTIFY. Same-process listeners pick up the local
    /// bump immediately; remote instances receive the notification through
    /// their PgListener and bump their own atomic.
    async fn bump_context(&self, workspace_id: WorkspaceId) {
        self.context_generation.bump();
        if let Err(e) = sqlx::query("SELECT pg_notify('context_changed', $1)")
            .bind(workspace_id.to_string())
            .execute(&self.pool)
            .await
        {
            tracing::warn!(error = %e, "pg_notify(context_changed) failed");
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
        if content.len() > MAX_NOTE_CONTENT_LEN {
            return Err(NoteError::ContentTooLarge(content.len()));
        }
        sub.can(AuthVerb::Create, AuthResource::Note(workspace_id, None))?;

        let mut op = self.repo.begin_op().await?.with_db_time().await?;

        let note_id = NoteId::new();
        let created_at = op.now().to_rfc3339();
        let runtime_file = RuntimeFile::for_note(
            note_id,
            workspace_id,
            workspace_name,
            &title,
            &content,
            &tags,
            &created_at,
            &created_at,
        );
        let file_hash = runtime_file.file_hash();

        let new_note = NewNote::builder()
            .id(note_id)
            .workspace_id(workspace_id)
            .workspace_name(workspace_name)
            .title(&title)
            .content(&content)
            .tags(tags)
            .file_hash(file_hash)
            .build()
            .expect("NewNote builder should not fail");

        let note = self.repo.create_in_op(&mut op, new_note).await?;
        op.commit().await?;

        self.bump_context(workspace_id).await;
        Ok(note)
    }

    /// Update an existing note.
    #[instrument(name = "note.update", skip(self))]
    pub async fn update(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
        note_id: NoteId,
        title: String,
        content: String,
        tags: Vec<String>,
    ) -> Result<Note, NoteError> {
        if content.len() > MAX_NOTE_CONTENT_LEN {
            return Err(NoteError::ContentTooLarge(content.len()));
        }
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

        let mut op = self.repo.begin_op().await?.with_db_time().await?;
        let updated_at = op.now().to_rfc3339();

        let created_at = note.created_at();
        let runtime_file = RuntimeFile::for_note(
            note.id,
            note.workspace_id,
            &note.workspace_name,
            &title,
            &content,
            &tags,
            &created_at,
            &updated_at,
        );
        let file_hash = runtime_file.file_hash();

        note.update(title, content, tags, file_hash);

        self.repo.update_in_op(&mut op, &mut note).await?;
        op.commit().await?;

        self.bump_context(workspace_id).await;
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
                self.update(sub, workspace_id, id, title, content, tags)
                    .await
            }
            None => {
                self.store(sub, workspace_id, workspace_name, title, content, tags)
                    .await
            }
        }
    }

    /// Pin a note so it appears in workspace context injection.
    #[instrument(name = "note.pin", skip(self))]
    pub async fn pin(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
        note_id: NoteId,
    ) -> Result<Note, NoteError> {
        let mut note = self.repo.find_by_id(note_id).await?;
        if note.workspace_id != workspace_id {
            return Err(NoteError::Authorization(AuthorizationError::Forbidden {
                verb: AuthVerb::Update,
                resource: AuthResource::Workspace(Some(workspace_id)),
            }));
        }
        sub.can(
            AuthVerb::Update,
            AuthResource::Note(workspace_id, Some(note_id)),
        )?;
        if note.pin().did_execute() {
            self.repo.update(&mut note).await?;
            self.bump_context(workspace_id).await;
        }
        Ok(note)
    }

    /// Unpin a note, removing it from workspace context injection.
    #[instrument(name = "note.unpin", skip(self))]
    pub async fn unpin(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
        note_id: NoteId,
    ) -> Result<Note, NoteError> {
        let mut note = self.repo.find_by_id(note_id).await?;
        if note.workspace_id != workspace_id {
            return Err(NoteError::Authorization(AuthorizationError::Forbidden {
                verb: AuthVerb::Update,
                resource: AuthResource::Workspace(Some(workspace_id)),
            }));
        }
        sub.can(
            AuthVerb::Update,
            AuthResource::Note(workspace_id, Some(note_id)),
        )?;
        if note.unpin().did_execute() {
            self.repo.update(&mut note).await?;
            self.bump_context(workspace_id).await;
        }
        Ok(note)
    }

    /// List pinned notes for a workspace.
    #[instrument(name = "note.list_pinned", skip(self))]
    pub async fn list_pinned(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<Note>, NoteError> {
        sub.can(AuthVerb::Read, AuthResource::Note(workspace_id, None))?;
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
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
        Ok(result.entities.into_iter().filter(|n| n.pinned).collect())
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
    ) -> Result<Vec<SearchResult>, NoteError> {
        sub.can(AuthVerb::Read, AuthResource::Note(workspace_id, None))?;
        self.library
            .search(
                uuid::Uuid::from(workspace_id),
                query,
                Some(DocType::Note),
                limit,
            )
            .await
            .map_err(NoteError::from)
    }

    /// List all notes in a workspace.
    #[instrument(name = "note.list", skip(self))]
    pub async fn list(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
        limit: usize,
    ) -> Result<Vec<Note>, NoteError> {
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
        Ok(result.entities)
    }

    /// Bulk soft-delete all notes belonging to a workspace within a
    /// transaction. Used during workspace cascade deletion.
    #[instrument(name = "note.delete_for_workspace_in_op", skip_all)]
    pub(crate) async fn delete_for_workspace_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        workspace_id: WorkspaceId,
    ) -> Result<(), NoteError> {
        self.repo
            .cascade_delete_for_workspace_in_op(op, workspace_id)
            .await?;
        Ok(())
    }

    /// Maximum total characters of pinned note content to inject into an
    /// agent's system prompt. Notes are included most-recently-updated-first;
    /// once this budget is exhausted, remaining pinned notes are omitted with
    /// a hint to use the `notes search` tool.
    const PINNED_INJECTION_BUDGET: usize = 8000;

    /// Maximum number of non-pinned notes to include in the title/tag index.
    const NOTE_INDEX_LIMIT: usize = 20;

    /// Build workspace notes context for system prompt injection.
    ///
    /// Contains two sections:
    /// 1. **Pinned notes** — full content, within the character budget.
    /// 2. **Recent notes index** — title + tags only for non-pinned notes,
    ///    so the agent knows what's available to search for.
    ///
    /// Returns `None` if the workspace has no notes at all.
    /// This is an internal method (no auth check) — called at agent creation.
    #[instrument(name = "note.pinned_context_for_workspace", skip(self))]
    pub async fn pinned_context_for_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Option<String>, NoteError> {
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
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

        if result.entities.is_empty() {
            return Ok(None);
        }

        let mut pinned: Vec<&Note> = result.entities.iter().filter(|n| n.pinned).collect();
        let non_pinned: Vec<&Note> = result.entities.iter().filter(|n| !n.pinned).collect();

        let header = "# Workspace Notes\n\n\
             Use the `notes` tool with command `search` to retrieve full content.\n";
        let mut buf = String::from(header);
        let mut remaining = Self::PINNED_INJECTION_BUDGET.saturating_sub(header.len());

        // ── Section 1: Pinned notes (full content) ──────────────────────
        if !pinned.is_empty() {
            // Sort by most-recently-updated first (events timestamp).
            pinned.sort_by(|a, b| {
                let a_ts = a.events.entity_last_modified_at();
                let b_ts = b.events.entity_last_modified_at();
                b_ts.cmp(&a_ts)
            });

            let section_header = "\n## Pinned\n";
            buf.push_str(section_header);
            remaining = remaining.saturating_sub(section_header.len());
            let mut included = 0;
            let total = pinned.len();

            for note in &pinned {
                let entry = format!("\n### {}\n{}\n", note.title, note.content);
                if entry.len() > remaining {
                    break;
                }
                buf.push_str(&entry);
                remaining -= entry.len();
                included += 1;
            }

            if included < total {
                buf.push_str(&format!(
                    "\n({} more pinned note(s) omitted — use `notes search` to find them)\n",
                    total - included,
                ));
            }
        }

        // ── Section 2: Recent notes index (titles + tags only) ──────────
        if !non_pinned.is_empty() && remaining > 100 {
            let section_header = "\n## Recent notes\n\n";
            buf.push_str(section_header);
            remaining = remaining.saturating_sub(section_header.len());

            let mut indexed = 0;
            for note in non_pinned.iter().take(Self::NOTE_INDEX_LIMIT) {
                let line = if note.tags.is_empty() {
                    format!("- {}\n", note.title)
                } else {
                    format!("- {} [{}]\n", note.title, note.tags.join(", "))
                };
                if line.len() > remaining {
                    break;
                }
                buf.push_str(&line);
                remaining -= line.len();
                indexed += 1;
            }

            let remaining_count = non_pinned.len() - indexed;
            if remaining_count > 0 {
                buf.push_str(&format!("- ... and {remaining_count} more\n"));
            }
        }

        Ok(Some(buf))
    }
}
