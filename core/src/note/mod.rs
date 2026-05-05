mod entity;
pub mod error;
pub mod file;
pub mod importer;
pub(crate) mod repo;

use drua_library::{CommitAttribution, SearchHit, WriteOp};
use es_entity::AtomicOperation;
use tracing::instrument;

pub use entity::Note;
use entity::*;
pub use error::*;
pub use file::{parse_note_markdown, ParsedNote};
pub use importer::NotesImporter;
use repo::*;

pub const NOTE_DOC_TYPE: drua_library::DocType = drua_library::DocType::new("note");

use crate::agent::AgentScope;
use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::library::SpaceMounts;
use crate::note::file::render_note_markdown_with_pin;
use crate::primitives::*;
use crate::user::Users;
use std::sync::Arc;

/// Resolved scope for `Notes::import_from_library`. Mirrors `skill::ImportScope`.
#[derive(Debug, Clone)]
pub enum ImportScope {
    Project {
        project_id: ProjectId,
        project_name: String,
    },
    Space {
        space_id: SpaceId,
        space_slug: String,
    },
}

impl ImportScope {
    fn bump_scope(&self) -> ScopeId {
        match self {
            ImportScope::Project { project_id, .. } => ScopeId::Project(*project_id),
            ImportScope::Space { space_id, .. } => ScopeId::Space(*space_id),
        }
    }
}

#[derive(Clone)]
pub struct Notes {
    repo: NoteRepo,
    library: drua_library::Library,
    pool: sqlx::PgPool,
    users: Arc<Users>,
    /// Read facade for the project↔space mount relationship — held
    /// alongside the repo so `pinned_context_for_scope` can resolve
    /// space-mounted notes internally without callers passing the
    /// scope through.
    space_mounts: Arc<SpaceMounts>,
    context_generation: ContextGeneration,
}

impl Notes {
    pub fn new(
        pool: &sqlx::PgPool,
        library: drua_library::Library,
        space_mounts: Arc<SpaceMounts>,
        users: Arc<Users>,
        context_generation: ContextGeneration,
    ) -> Self {
        Self {
            repo: NoteRepo::new(pool, library.clone()),
            library,
            pool: pool.clone(),
            users,
            space_mounts,
            context_generation,
        }
    }

    /// Registers a `ContextBumpHook` so committing the op bumps the local
    /// `ContextGeneration` and fires a `context_changed` PG NOTIFY. All
    /// mutations go through this helper rather than `repo.begin_op()`.
    async fn begin_op(&self, scope: ScopeId) -> Result<es_entity::DbOp<'static>, NoteError> {
        let mut op = self.repo.begin_op().await?;
        let hook = ContextBumpHook::new(self.context_generation.clone(), self.pool.clone(), scope);
        op.add_commit_hook(hook)
            .expect("DbOp supports commit hooks");
        Ok(op)
    }

    /// Composable variant: registers the bump hook on the caller's op.
    pub(crate) fn register_context_bump<OP: AtomicOperation>(&self, op: &mut OP, scope: ScopeId) {
        let hook = ContextBumpHook::new(self.context_generation.clone(), self.pool.clone(), scope);
        if op.add_commit_hook(hook).is_err() {
            tracing::warn!("AtomicOperation rejected ContextBumpHook; context bump skipped");
        }
    }

    /// Wipe the search-store row in the same DbOp and queue a
    /// [`WriteOp::DeleteFile`] so the upstream git repo loses the
    /// markdown on the next library write tick. Mirrors
    /// `Skills::enqueue_skill_delete_in_op`.
    async fn enqueue_note_delete_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        note: &Note,
        attribution: CommitAttribution,
    ) -> Result<(), NoteError> {
        let id_uuid: uuid::Uuid = note.id.into();
        let message = format!("note: delete {}-{}", &note.title, &id_uuid.to_string()[..8]);
        self.library
            .enqueue_full_in_op(
                op,
                None,
                vec![(id_uuid, NOTE_DOC_TYPE)],
                Some(WriteOp::DeleteFile {
                    path: note.path.clone(),
                    message,
                }),
                attribution,
            )
            .await?;
        Ok(())
    }

    #[instrument(name = "note.store", skip(self))]
    pub async fn store(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        project_name: &str,
        title: String,
        content: String,
        tags: Vec<String>,
    ) -> Result<Note, NoteError> {
        if content.len() > MAX_NOTE_CONTENT_LEN {
            return Err(NoteError::ContentTooLarge(content.len()));
        }
        sub.can(AuthVerb::Create, AuthResource::Note(project_id, None))?;

        let mut op = self
            .begin_op(ScopeId::Project(project_id))
            .await?
            .with_db_time()
            .await?;

        let note_id = NoteId::new();
        // New project notes default to unpinned (admin `pin`/`unpin` is
        // the surface for flipping the bit afterwards). `path` is
        // derived inside `NewNoteBuilder::build()` from the
        // `(title, project_name, space_slug)` triple.
        let new_note = NewNote::builder()
            .id(note_id)
            .project_id(project_id)
            .project_name(project_name.to_string())
            .title(&title)
            .content(&content)
            .tags(tags)
            .build()
            .expect("NewNote builder should not fail");

        self.users.commit_attribution().await;
        let note = self.repo.create_in_op(&mut op, new_note).await?;
        op.commit().await?;
        Ok(note)
    }

    #[instrument(name = "note.update", skip(self))]
    pub async fn update(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        note_id: NoteId,
        title: String,
        content: String,
        tags: Vec<String>,
    ) -> Result<Note, NoteError> {
        if content.len() > MAX_NOTE_CONTENT_LEN {
            return Err(NoteError::ContentTooLarge(content.len()));
        }
        sub.can(
            AuthVerb::Update,
            AuthResource::Note(project_id, Some(note_id)),
        )?;

        let mut op = self
            .begin_op(ScopeId::Project(project_id))
            .await?
            .with_db_time()
            .await?;
        let mut note = self.repo.find_by_id_in_op(&mut op, note_id).await?;
        if note.project_id != Some(project_id) {
            return Err(NoteError::Authorization(AuthorizationError::Forbidden {
                verb: AuthVerb::Read,
                resource: AuthResource::Project(Some(project_id)),
            }));
        }
        let updated_at = op.now().to_rfc3339();

        let created_at = note.created_at();
        // Render with the entity's current pin state so the
        // hash compare against `note.file_hash()` is apples-to-apples.
        let rendered = render_note_markdown_with_pin(
            note.id.into(),
            &title,
            &content,
            &tags,
            &created_at,
            &updated_at,
            note.pinned,
        );
        let file_hash = drua_library::GitFileHash::new(rendered);

        if note.update(title, content, tags, file_hash).did_execute() {
            self.users.commit_attribution().await;
            self.repo.update_in_op(&mut op, &mut note).await?;
        }
        op.commit().await?;
        Ok(note)
    }

    /// `Some(id)` updates; `None` creates.
    #[allow(clippy::too_many_arguments)]
    #[instrument(name = "note.store_or_update", skip(self))]
    pub async fn store_or_update(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        project_name: &str,
        note_id: Option<NoteId>,
        title: String,
        content: String,
        tags: Vec<String>,
    ) -> Result<Note, NoteError> {
        match note_id {
            Some(id) => self.update(sub, project_id, id, title, content, tags).await,
            None => {
                self.store(sub, project_id, project_name, title, content, tags)
                    .await
            }
        }
    }

    #[instrument(name = "note.pin", skip(self))]
    pub async fn pin(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        note_id: NoteId,
    ) -> Result<Note, NoteError> {
        sub.can(
            AuthVerb::Update,
            AuthResource::Note(project_id, Some(note_id)),
        )?;
        let mut op = self.begin_op(ScopeId::Project(project_id)).await?;
        let mut note = self.repo.find_by_id_in_op(&mut op, note_id).await?;
        if note.project_id != Some(project_id) {
            return Err(NoteError::Authorization(AuthorizationError::Forbidden {
                verb: AuthVerb::Update,
                resource: AuthResource::Project(Some(project_id)),
            }));
        }
        if note.pin().did_execute() {
            self.repo.update_in_op(&mut op, &mut note).await?;
            op.commit().await?;
        }
        Ok(note)
    }

    #[instrument(name = "note.unpin", skip(self))]
    pub async fn unpin(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        note_id: NoteId,
    ) -> Result<Note, NoteError> {
        sub.can(
            AuthVerb::Update,
            AuthResource::Note(project_id, Some(note_id)),
        )?;
        let mut op = self.begin_op(ScopeId::Project(project_id)).await?;
        let mut note = self.repo.find_by_id_in_op(&mut op, note_id).await?;
        if note.project_id != Some(project_id) {
            return Err(NoteError::Authorization(AuthorizationError::Forbidden {
                verb: AuthVerb::Update,
                resource: AuthResource::Project(Some(project_id)),
            }));
        }
        if note.unpin().did_execute() {
            self.repo.update_in_op(&mut op, &mut note).await?;
            op.commit().await?;
        }
        Ok(note)
    }

    #[instrument(name = "note.list_pinned", skip(self))]
    pub async fn list_pinned(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
    ) -> Result<Vec<Note>, NoteError> {
        sub.can(AuthVerb::Read, AuthResource::Note(project_id, None))?;
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_project_id_by_created_at(
                Some(project_id),
                query,
                es_entity::ListDirection::Descending,
            )
            .await?;
        Ok(result.entities.into_iter().filter(|n| n.pinned).collect())
    }

    #[instrument(name = "note.find_by_id", skip(self))]
    pub async fn find_by_id(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        note_id: NoteId,
    ) -> Result<Note, NoteError> {
        sub.can(
            AuthVerb::Read,
            AuthResource::Note(project_id, Some(note_id)),
        )?;
        let note = self.repo.find_by_id(note_id).await?;
        if note.project_id != Some(project_id) {
            return Err(NoteError::Authorization(AuthorizationError::Forbidden {
                verb: AuthVerb::Read,
                resource: AuthResource::Project(Some(project_id)),
            }));
        }
        Ok(note)
    }

    #[instrument(name = "note.search", skip(self))]
    pub async fn search(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>, NoteError> {
        sub.can(AuthVerb::Read, AuthResource::Note(project_id, None))?;
        Ok(self
            .library
            .search()
            .search(
                query,
                None,
                &[uuid::Uuid::from(project_id)],
                &[NOTE_DOC_TYPE],
                &[],
                limit,
            )
            .await?)
    }

    #[instrument(name = "note.list", skip(self))]
    pub async fn list(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        limit: usize,
    ) -> Result<Vec<Note>, NoteError> {
        sub.can(AuthVerb::Read, AuthResource::Note(project_id, None))?;
        let query = es_entity::PaginatedQueryArgs {
            first: limit,
            after: None,
        };
        let result = self
            .repo
            .list_for_project_id_by_created_at(
                Some(project_id),
                query,
                es_entity::ListDirection::Descending,
            )
            .await?;
        Ok(result.entities)
    }

    /// Soft-deletes the note row and enqueues a [`WriteOp::DeleteFile`]
    /// for the canonical markdown path so the next library write tick
    /// removes it from the upstream git repo. Space-scoped notes are
    /// rejected — callers must route through the spaces tool to keep
    /// the project / space surfaces cleanly separated.
    #[instrument(name = "note.delete", skip(self))]
    pub async fn delete(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        note_id: NoteId,
    ) -> Result<(), NoteError> {
        sub.can(
            AuthVerb::Delete,
            AuthResource::Note(project_id, Some(note_id)),
        )?;
        Audit::record_action_if_unset("note.delete");
        Audit::record_project_id(project_id);
        let mut op = self.begin_op(ScopeId::Project(project_id)).await?;
        let note = self.repo.find_by_id_in_op(&mut op, note_id).await?;
        if note.space_id.is_some() {
            return Err(NoteError::SpaceScopedDeleteViaSpaces);
        }
        if note.project_id != Some(project_id) {
            return Err(NoteError::Authorization(AuthorizationError::Forbidden {
                verb: AuthVerb::Delete,
                resource: AuthResource::Note(project_id, Some(note_id)),
            }));
        }
        let attribution = self.users.commit_attribution().await;
        self.enqueue_note_delete_in_op(&mut op, &note, attribution)
            .await?;
        self.repo.delete_in_op(&mut op, note).await?;
        op.commit().await?;
        Ok(())
    }

    /// Used during project cascade deletion.
    #[instrument(name = "note.delete_for_project_in_op", skip_all)]
    pub(crate) async fn delete_for_project_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        project_id: ProjectId,
    ) -> Result<(), NoteError> {
        self.repo
            .cascade_delete_for_project_in_op(op, project_id)
            .await?;
        Ok(())
    }

    /// Reverse-sync delete: soft-deletes the note whose canonical
    /// markdown lives at `path` and bumps the appropriate
    /// `ContextGeneration`. Mirrors `Skills::delete_by_path_in_op`.
    pub(crate) async fn delete_by_path_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        path: &str,
    ) -> Result<Option<NoteId>, NoteError> {
        let Some(note) = self.repo.maybe_find_by_path_in_op(&mut *op, path).await? else {
            return Ok(None);
        };
        let id = note.id;
        let scope = match (note.space_id, note.project_id) {
            (Some(sid), _) => ScopeId::Space(sid),
            (None, Some(pid)) => ScopeId::Project(pid),
            (None, None) => ScopeId::Global,
        };
        self.library
            .search()
            .delete_in_op(op, id.into(), NOTE_DOC_TYPE)
            .await?;
        self.repo.delete_in_op(op, note).await?;
        self.register_context_bump(op, scope);
        Ok(Some(id))
    }

    /// Reverse-sync entry point: persist a `ParsedNote` produced by the
    /// library importer. Creates, updates, or revives depending on
    /// whether the note already exists. Content / pin / path deltas
    /// are reconciled as separate events so an external move
    /// (`spaces edit op=move`) doesn't spuriously fire an `Updated`.
    pub(crate) async fn import_from_library<OP: AtomicOperation>(
        &self,
        op: &mut OP,
        parsed: ParsedNote,
        scope: ImportScope,
    ) -> Result<Option<Note>, NoteError> {
        let file_hash = parsed.file_hash();

        // Live row → reconcile.
        if let Some(existing) = self
            .repo
            .maybe_find_by_id_in_op(&mut *op, parsed.note_id)
            .await?
        {
            return reconcile_existing_note(self, op, existing, parsed, file_hash, scope).await;
        }

        // Soft-deleted row at the same id? `spaces edit op=move`
        // delivers delete-old-path → soft-delete; the matching
        // add-new-path then re-imports the same frontmatter id. Revive
        // instead of `create_in_op` (which would PK-conflict on the
        // soft-deleted row).
        if self
            .repo
            .maybe_revive_in_op(&mut *op, parsed.note_id)
            .await?
        {
            let existing = self.repo.find_by_id_in_op(&mut *op, parsed.note_id).await?;
            return reconcile_existing_note(self, op, existing, parsed, file_hash, scope).await;
        }

        // Truly new — fresh create.
        let mut builder = NewNote::builder();
        builder = builder
            .id(parsed.note_id)
            .title(parsed.title)
            .content(parsed.content)
            .tags(parsed.tags)
            .pinned(parsed.pinned)
            .path(parsed.path);
        match &scope {
            ImportScope::Project {
                project_id,
                project_name,
            } => {
                builder = builder
                    .project_id(*project_id)
                    .project_name(project_name.clone());
            }
            ImportScope::Space {
                space_id,
                space_slug,
            } => {
                builder = builder.space_id(*space_id).space_slug(space_slug.clone());
            }
        }
        let new = builder
            .build()
            .map_err(|e| NoteError::BuildEntity(e.to_string()))?;
        let note = self.repo.create_in_op(op, new).await?;
        self.register_context_bump(op, scope.bump_scope());
        Ok(Some(note))
    }

    /// Char budget for pinned-note injection.
    const PINNED_INJECTION_BUDGET: usize = 8000;

    const NOTE_INDEX_LIMIT: usize = 20;

    /// Two sections: pinned notes (full content within budget) and a recent
    /// notes index (title+tags only). `None` if neither the project nor
    /// any of its mounted spaces have notes. Internal — no auth check,
    /// called at agent creation.
    ///
    /// Notes from mounted spaces appear alongside project notes in both
    /// sections. The pinned bit is global to the note — pinning a space
    /// note pins it for every mounting project; unpinning does the
    /// reverse.
    #[instrument(name = "note.pinned_context_for_scope", skip(self, scope))]
    pub async fn pinned_context_for_scope(
        &self,
        scope: &AgentScope,
    ) -> Result<Option<String>, NoteError> {
        let project_notes = self
            .repo
            .list_for_project_id_by_created_at(
                Some(scope.project_id),
                es_entity::PaginatedQueryArgs {
                    first: 100,
                    after: None,
                },
                es_entity::ListDirection::Descending,
            )
            .await?
            .entities;

        let mut space_notes: Vec<Note> = Vec::new();
        for space_id in &scope.mounted_space_ids {
            let res = self
                .repo
                .list_for_space_id_by_created_at(
                    Some(*space_id),
                    es_entity::PaginatedQueryArgs {
                        first: 100,
                        after: None,
                    },
                    es_entity::ListDirection::Descending,
                )
                .await?;
            space_notes.extend(res.entities);
        }

        if project_notes.is_empty() && space_notes.is_empty() {
            return Ok(None);
        }

        let mut pinned: Vec<&Note> = project_notes.iter().filter(|n| n.pinned).collect();
        pinned.extend(space_notes.iter().filter(|n| n.pinned));
        let mut non_pinned: Vec<&Note> = project_notes.iter().filter(|n| !n.pinned).collect();
        non_pinned.extend(space_notes.iter().filter(|n| !n.pinned));

        let header = "# Project Notes\n\n\
             Use the `notes` tool with command `search` to retrieve full content.\n";
        let mut buf = String::from(header);
        let mut remaining = Self::PINNED_INJECTION_BUDGET.saturating_sub(header.len());

        if !pinned.is_empty() {
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
                let scope_tag = if note.space_slug.is_some() {
                    " [space]"
                } else {
                    ""
                };
                let entry = format!("\n### {}{}\n{}\n", note.title, scope_tag, note.content);
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

    /// Read facade (for diagnostic / admin tools).
    pub fn space_mounts(&self) -> &Arc<SpaceMounts> {
        &self.space_mounts
    }
}

/// Reconcile content (Updated event), pin (Pinned/Unpinned) and path
/// (PathChanged event) on an existing live note against an incoming
/// `ParsedNote` from the reverse-sync importer. `Ok(None)` when no
/// delta produced an event. Free function so the live and revival
/// branches in `import_from_library` can share it without fighting
/// the borrow checker over `&mut self`.
async fn reconcile_existing_note<OP: AtomicOperation>(
    notes: &Notes,
    op: &mut OP,
    mut existing: Note,
    parsed: ParsedNote,
    file_hash: drua_library::GitFileHash,
    scope: ImportScope,
) -> Result<Option<Note>, NoteError> {
    let content_changed = existing
        .update(
            parsed.title.clone(),
            parsed.content.clone(),
            parsed.tags.clone(),
            file_hash,
        )
        .did_execute();
    let pin_changed = if parsed.pinned != existing.pinned {
        if parsed.pinned {
            existing.pin().did_execute()
        } else {
            existing.unpin().did_execute()
        }
    } else {
        false
    };
    let path_changed = existing.change_path(parsed.path).did_execute();
    if !content_changed && !pin_changed && !path_changed {
        return Ok(None);
    }
    notes.repo.update_in_op(op, &mut existing).await?;
    notes.register_context_bump(op, scope.bump_scope());
    Ok(Some(existing))
}
