mod entity;
pub mod error;
pub mod file;
pub(crate) mod repo;

use es_entity::AtomicOperation;
use tracing::instrument;

pub use entity::Note;
use entity::*;
pub use error::*;
use repo::*;

use crate::auth::AuthSubject;
use crate::library::{DocType, Library, SearchResult, UpstreamOp};
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

    /// Registers a `ContextBumpHook` so committing the op bumps the local
    /// `ContextGeneration` and fires a `context_changed` PG NOTIFY. All
    /// mutations go through this helper rather than `repo.begin_op()`.
    async fn begin_op(&self, project_id: ProjectId) -> Result<es_entity::DbOp<'static>, NoteError> {
        let mut op = self.repo.begin_op().await?;
        let hook = ContextBumpHook::new(
            self.context_generation.clone(),
            self.pool.clone(),
            Some(project_id),
        );
        op.add_commit_hook(hook)
            .expect("DbOp supports commit hooks");
        Ok(op)
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

        let mut op = self.begin_op(project_id).await?.with_db_time().await?;

        let note_id = NoteId::new();
        let created_at = op.now().to_rfc3339();
        let runtime_file = UpstreamOp::for_note(
            note_id,
            project_id,
            project_name,
            &title,
            &content,
            &tags,
            &created_at,
            &created_at,
        );
        let file_hash = runtime_file.file_hash();

        let new_note = NewNote::builder()
            .id(note_id)
            .project_id(project_id)
            .project_name(project_name)
            .title(&title)
            .content(&content)
            .tags(tags)
            .file_hash(file_hash)
            .build()
            .expect("NewNote builder should not fail");

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

        let mut op = self.begin_op(project_id).await?.with_db_time().await?;
        let mut note = self.repo.find_by_id_in_op(&mut op, note_id).await?;
        if note.project_id != project_id {
            return Err(NoteError::Authorization(AuthorizationError::Forbidden {
                verb: AuthVerb::Read,
                resource: AuthResource::Project(Some(project_id)),
            }));
        }
        let updated_at = op.now().to_rfc3339();

        let created_at = note.created_at();
        let runtime_file = UpstreamOp::for_note(
            note.id,
            note.project_id,
            &note.project_name,
            &title,
            &content,
            &tags,
            &created_at,
            &updated_at,
        );
        let file_hash = runtime_file.file_hash();

        if note.update(title, content, tags, file_hash).did_execute() {
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
        let mut op = self.begin_op(project_id).await?;
        let mut note = self.repo.find_by_id_in_op(&mut op, note_id).await?;
        if note.project_id != project_id {
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
        let mut op = self.begin_op(project_id).await?;
        let mut note = self.repo.find_by_id_in_op(&mut op, note_id).await?;
        if note.project_id != project_id {
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
                project_id,
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
        if note.project_id != project_id {
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
    ) -> Result<Vec<SearchResult>, NoteError> {
        sub.can(AuthVerb::Read, AuthResource::Note(project_id, None))?;
        self.library
            .search(
                uuid::Uuid::from(project_id),
                query,
                Some(DocType::Note),
                limit,
            )
            .await
            .map_err(NoteError::from)
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
                project_id,
                query,
                es_entity::ListDirection::Descending,
            )
            .await?;
        Ok(result.entities)
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

    /// Char budget for pinned-note injection. Notes are included
    /// most-recently-updated-first; remaining pinned notes are omitted
    /// with a hint to use `notes search`.
    const PINNED_INJECTION_BUDGET: usize = 8000;

    const NOTE_INDEX_LIMIT: usize = 20;

    /// Two sections: pinned notes (full content within budget) and a recent
    /// notes index (title+tags only). `None` if the project has no notes.
    /// Internal — no auth check, called at agent creation.
    #[instrument(name = "note.pinned_context_for_project", skip(self))]
    pub async fn pinned_context_for_project(
        &self,
        project_id: ProjectId,
    ) -> Result<Option<String>, NoteError> {
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_project_id_by_created_at(
                project_id,
                query,
                es_entity::ListDirection::Descending,
            )
            .await?;

        if result.entities.is_empty() {
            return Ok(None);
        }

        let mut pinned: Vec<&Note> = result.entities.iter().filter(|n| n.pinned).collect();
        let non_pinned: Vec<&Note> = result.entities.iter().filter(|n| !n.pinned).collect();

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
