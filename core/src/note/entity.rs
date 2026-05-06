use derive_builder::Builder;
use drua_library::{GitFileHash, SearchableFields, WriteOp};
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::note::file::render_note_markdown_with_pin;
use crate::note::NOTE_DOC_TYPE;
use crate::primitives::*;
use crate::skill::file::slugify;

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "NoteId")]
pub enum NoteEvent {
    Initialized {
        id: NoteId,
        #[serde(default)]
        project_id: Option<ProjectId>,
        #[serde(default)]
        project_name: Option<String>,
        /// `Some(s)` for space-scoped notes; mutually exclusive with
        /// `project_id` (enforced by the `notes_owner_exactly_one`
        /// CHECK constraint).
        #[serde(default)]
        space_id: Option<SpaceId>,
        /// Denormalised space slug — mirrors the `project_name` denorm.
        #[serde(default)]
        space_slug: Option<String>,
        title: String,
        content: String,
        tags: Vec<String>,
        /// Repo-relative on-disk path. Sacred — never mutated by the
        /// importer.
        path: String,
        /// Initial pinned state. Set from frontmatter (`pinned: true`)
        /// when a space note is imported; `false` for project notes.
        pinned: bool,
    },
    Updated {
        title: String,
        content: String,
        tags: Vec<String>,
    },
    Pinned {},
    Unpinned {},
    /// Records a file move detected by reverse-sync — the importer
    /// re-imports a previously-known frontmatter id at a new on-disk
    /// path. Distinct from `Updated` because file content didn't
    /// change, only its location; not a content event so no library
    /// write-back is fired (the move happened in git already).
    PathChanged {
        path: String,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Note {
    pub id: NoteId,
    #[builder(default)]
    pub(crate) project_id: Option<ProjectId>,
    #[builder(default)]
    pub(crate) project_name: Option<String>,
    /// `Some(s)` when this note belongs to a space rather than a project.
    /// Mutually exclusive with `project_id` (DB CHECK constraint).
    #[builder(default)]
    pub(crate) space_id: Option<SpaceId>,
    /// Denormalised space slug — `Some` exactly when `space_id` is `Some`.
    #[builder(default)]
    pub(crate) space_slug: Option<String>,
    pub(crate) title: String,
    pub(crate) content: String,
    pub(crate) tags: Vec<String>,
    pub(crate) pinned: bool,
    /// Repo-relative on-disk path. The importer never mutates this —
    /// whatever the user wrote is the path of record.
    pub path: String,
    pub(super) events: EntityEvents<NoteEvent>,
}

impl Note {
    /// Document content with frontmatter, as stored in the library.
    pub fn content(&self) -> String {
        self.rendered()
    }

    /// Note title (first line / heading).
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Markdown body of the note (no frontmatter).
    pub fn body(&self) -> &str {
        &self.content
    }

    /// Tags applied to the note.
    pub fn tags(&self) -> &[String] {
        &self.tags
    }

    /// Whether the note is pinned (injected into agents' `<notes>` block).
    /// The bit is shared globally — pinning a space note pins it for
    /// every project that mounts the space.
    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    pub fn created_at(&self) -> String {
        self.events
            .entity_first_persisted_at()
            .map(|t| t.to_rfc3339())
            .unwrap_or_default()
    }

    /// Canonical on-disk content (markdown w/ frontmatter, including
    /// `pinned: true` when [`Self::is_pinned`] is set).
    pub(super) fn rendered(&self) -> String {
        render_note_markdown_with_pin(
            self.id.into(),
            &self.title,
            &self.content,
            &self.tags,
            &self.created_at(),
            &self.updated_at_rfc3339(),
            self.pinned,
        )
    }

    /// Hash of the canonical runtime form, used for reverse-sync
    /// idempotency comparisons. Computed on demand so pin/unpin events
    /// stay consistent with on-disk content (mirrors `Skill::file_hash`).
    pub(crate) fn file_hash(&self) -> GitFileHash {
        GitFileHash::new(self.rendered())
    }

    fn updated_at_rfc3339(&self) -> String {
        self.events
            .entity_last_modified_at()
            .or_else(|| self.events.entity_first_persisted_at())
            .map(|t| t.to_rfc3339())
            .unwrap_or_default()
    }

    /// Set pin state to `pinned`. Idempotent — calling with a value
    /// equal to the current state pushes no event and returns
    /// `AlreadyApplied`. The two branches inline the
    /// `idempotency_guard!` matchers because the macro short-circuits
    /// the enclosing function on `already_applied`, so the guard has
    /// to live at the same lexical level as the early-return.
    pub fn update_pin(&mut self, pinned: bool) -> Idempotent<()> {
        if pinned {
            idempotency_guard!(
                self.events.iter_all().rev(),
                already_applied: NoteEvent::Pinned {},
                resets_on: NoteEvent::Unpinned { .. }
            );
            self.pinned = true;
            self.events.push(NoteEvent::Pinned {});
        } else {
            idempotency_guard!(
                self.events.iter_all().rev(),
                already_applied: NoteEvent::Unpinned {},
                resets_on: NoteEvent::Pinned { .. }
            );
            self.pinned = false;
            self.events.push(NoteEvent::Unpinned {});
        }
        Idempotent::Executed(())
    }

    /// Records a path change on the entity. Idempotent — same path is
    /// a no-op. Only invoked by reverse-sync when a note is re-imported
    /// at a new on-disk location (e.g. user moved the markdown via
    /// `spaces edit op=move`). Pure metadata; doesn't fire a library
    /// write-back.
    pub fn change_path(&mut self, new_path: String) -> Idempotent<()> {
        if self.path == new_path {
            return Idempotent::AlreadyApplied;
        }
        self.path = new_path.clone();
        self.events.push(NoteEvent::PathChanged { path: new_path });
        Idempotent::Executed(())
    }

    pub fn update(
        &mut self,
        title: String,
        content: String,
        tags: Vec<String>,
        incoming_file_hash: GitFileHash,
    ) -> Idempotent<()> {
        if self.file_hash() == incoming_file_hash {
            return Idempotent::AlreadyApplied;
        }
        self.title = title.clone();
        self.content = content.clone();
        self.tags = tags.clone();
        self.events.push(NoteEvent::Updated {
            title,
            content,
            tags,
        });
        Idempotent::Executed(())
    }
}

impl drua_library::LibrarySynced for Note {
    type Event = NoteEvent;

    fn is_content_event(ev: &NoteEvent) -> bool {
        // Pin/unpin flips the `pinned: true` line in the frontmatter,
        // so they're content events. `PathChanged` is metadata-only —
        // the file move happened in git already, no write-back to fire.
        matches!(
            ev,
            NoteEvent::Initialized { .. }
                | NoteEvent::Updated { .. }
                | NoteEvent::Pinned {}
                | NoteEvent::Unpinned {}
        )
    }

    fn searchable_fields(&self) -> SearchableFields {
        let project_name = self.project_name.as_deref();
        let space_slug = self.space_slug.as_deref();
        let (scope_id, scope_slug) = match (self.space_id, self.project_id) {
            (Some(s), _) => (Some(uuid::Uuid::from(s)), space_slug.map(str::to_string)),
            (None, Some(p)) => (Some(uuid::Uuid::from(p)), project_name.map(str::to_string)),
            (None, None) => (None, None),
        };
        SearchableFields {
            doc_id: self.id.into(),
            doc_type: NOTE_DOC_TYPE,
            scope_id,
            scope_slug,
            name: self.title.clone(),
            path: Some(self.path.clone()),
            content: self.content.clone(),
        }
    }

    fn write_op(&self) -> WriteOp {
        let content = self.rendered().into_bytes();
        let id_uuid: uuid::Uuid = self.id.into();
        let message = format!(
            "note: {}-{}",
            slugify(&self.title),
            &id_uuid.to_string()[..8]
        );
        WriteOp::WriteFile {
            path: self.path.clone(),
            content,
            message,
        }
    }
}

impl std::fmt::Display for Note {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "id: {}\ntitle: {}", self.id, self.title)?;
        if self.pinned {
            write!(f, "\npinned: true")?;
        }
        if !self.tags.is_empty() {
            write!(f, "\ntags: {}", self.tags.join(", "))?;
        }
        Ok(())
    }
}

impl TryFromEvents<NoteEvent> for Note {
    fn try_from_events(events: EntityEvents<NoteEvent>) -> Result<Self, EntityHydrationError> {
        let mut builder = NoteBuilder::default();

        for event in events.iter_all() {
            match event {
                NoteEvent::Initialized {
                    id,
                    project_id,
                    project_name,
                    space_id,
                    space_slug,
                    title,
                    content,
                    tags,
                    path,
                    pinned,
                } => {
                    builder = builder
                        .id(*id)
                        .project_id(*project_id)
                        .project_name(project_name.clone())
                        .space_id(*space_id)
                        .space_slug(space_slug.clone())
                        .title(title.clone())
                        .content(content.clone())
                        .tags(tags.clone())
                        .pinned(*pinned)
                        .path(path.clone());
                }
                NoteEvent::Updated {
                    title,
                    content,
                    tags,
                } => {
                    builder = builder
                        .title(title.clone())
                        .content(content.clone())
                        .tags(tags.clone());
                }
                NoteEvent::Pinned {} => {
                    builder = builder.pinned(true);
                }
                NoteEvent::Unpinned {} => {
                    builder = builder.pinned(false);
                }
                NoteEvent::PathChanged { path } => {
                    builder = builder.path(path.clone());
                }
            }
        }

        builder.events(events).build()
    }
}

#[derive(Debug, Builder)]
#[builder(pattern = "owned", build_fn(name = "build_inner"))]
pub struct NewNote {
    #[builder(setter(into))]
    pub(super) id: NoteId,
    #[builder(default, setter(into, strip_option))]
    pub(super) project_id: Option<ProjectId>,
    #[builder(default, setter(into, strip_option))]
    pub(super) project_name: Option<String>,
    #[builder(default, setter(into, strip_option))]
    pub(super) space_id: Option<SpaceId>,
    #[builder(default, setter(into, strip_option))]
    pub(super) space_slug: Option<String>,
    #[builder(setter(into))]
    pub(super) title: String,
    #[builder(setter(into))]
    pub(super) content: String,
    #[builder(default)]
    pub(super) tags: Vec<String>,
    #[builder(default)]
    pub(super) pinned: bool,
    /// Repo-relative on-disk path. Filled lazily on `build()` via
    /// [`crate::note::file::default_note_path`] when the caller didn't
    /// set one — the reverse-sync importer always sets it
    /// (`parsed.path`); `Notes::store` skips it and lets the builder
    /// derive from `(title, project_name)`.
    #[builder(default, setter(into))]
    pub(super) path: String,
}

impl NewNote {
    pub fn builder() -> NewNoteBuilder {
        NewNoteBuilder::default().id(NoteId::new())
    }
}

impl NewNoteBuilder {
    /// Lazy default for `path` — derived from `(title, project_name,
    /// space_slug)` when the caller didn't set it.
    pub fn build(self) -> Result<NewNote, NewNoteBuilderError> {
        let mut me = self;
        if me.path.as_deref().map(str::is_empty).unwrap_or(true) {
            let title = me.title.as_deref().unwrap_or("");
            let project_name = me.project_name.as_ref().and_then(|p| p.as_deref());
            let space_slug = me.space_slug.as_ref().and_then(|s| s.as_deref());
            me.path = Some(crate::note::file::default_note_path(
                title,
                project_name,
                space_slug,
            ));
        }
        me.build_inner()
    }
}

impl IntoEvents<NoteEvent> for NewNote {
    fn into_events(self) -> EntityEvents<NoteEvent> {
        EntityEvents::init(
            self.id,
            [NoteEvent::Initialized {
                id: self.id,
                project_id: self.project_id,
                project_name: self.project_name,
                space_id: self.space_id,
                space_slug: self.space_slug,
                title: self.title,
                content: self.content,
                tags: self.tags,
                path: self.path,
                pinned: self.pinned,
            }],
        )
    }
}

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};

    use drua_library::GitFileHash;

    use crate::primitives::{NoteId, ProjectId};

    use super::{NewNote, Note};

    fn new_note() -> Note {
        let id = NoteId::new();
        let new = NewNote::builder()
            .id(id)
            .project_id(ProjectId::new())
            .project_name("test".to_string())
            .title("Test Note")
            .content("Some content here")
            .tags(vec!["tag1".into(), "tag2".into()])
            .path("runtime/projects/test/notes/test-note.md".to_string())
            .build()
            .unwrap();

        Note::try_from_events(new.into_events()).unwrap()
    }

    #[test]
    fn note_hydration() {
        let note = new_note();
        assert_eq!(note.title, "Test Note");
        assert_eq!(note.content, "Some content here");
        assert_eq!(note.tags, vec!["tag1", "tag2"]);
        assert_eq!(note.project_name.as_deref(), Some("test"));
        assert_eq!(note.path, "runtime/projects/test/notes/test-note.md");
    }

    #[test]
    fn note_update() {
        let mut note = new_note();
        // Compute the new content's hash so the idempotency guard treats
        // this as an actual change rather than no-op.
        let new_rendered = super::render_note_markdown_with_pin(
            note.id.into(),
            "Updated Title",
            "Updated content",
            &["new-tag".into()],
            &note.created_at(),
            &note.updated_at_rfc3339(),
            note.pinned,
        );
        let new_hash = GitFileHash::new(new_rendered);
        let res = note.update(
            "Updated Title".into(),
            "Updated content".into(),
            vec!["new-tag".into()],
            new_hash,
        );
        assert!(matches!(res, es_entity::Idempotent::Executed(())));
        assert_eq!(note.title, "Updated Title");
        assert_eq!(note.content, "Updated content");
        assert_eq!(note.tags, vec!["new-tag"]);
    }

    #[test]
    fn note_update_is_idempotent_when_rendered_matches_incoming() {
        let mut note = new_note();
        // Hash of current rendered content — guard must treat this as
        // already applied even when title/content/tags match what's
        // already there.
        let current_hash = note.file_hash();
        let res = note.update(
            note.title.clone(),
            note.content.clone(),
            note.tags.clone(),
            current_hash,
        );
        assert!(matches!(res, es_entity::Idempotent::AlreadyApplied));
    }

    #[test]
    fn note_pin_unpin() {
        let mut note = new_note();
        assert!(!note.pinned);

        let result = note.update_pin(true);
        assert!(matches!(result, es_entity::Idempotent::Executed(())));
        assert!(note.pinned);

        let result = note.update_pin(true);
        assert!(matches!(result, es_entity::Idempotent::AlreadyApplied));
        assert!(note.pinned);

        let result = note.update_pin(false);
        assert!(matches!(result, es_entity::Idempotent::Executed(())));
        assert!(!note.pinned);

        let result = note.update_pin(false);
        assert!(matches!(result, es_entity::Idempotent::AlreadyApplied));
        assert!(!note.pinned);
    }

    #[test]
    fn pin_round_trips_through_rendered_and_file_hash() {
        let mut note = new_note();
        let unpinned_hash = note.file_hash();

        let _ = note.update_pin(true);
        let pinned_hash = note.file_hash();
        assert_ne!(
            unpinned_hash, pinned_hash,
            "pin must change the canonical render"
        );
        assert!(
            note.rendered().contains("pinned: true"),
            "rendered output must include `pinned: true` when pinned"
        );

        let _ = note.update_pin(false);
        assert_eq!(
            note.file_hash(),
            unpinned_hash,
            "unpin must restore the original hash"
        );
        assert!(
            !note.rendered().contains("pinned: true"),
            "rendered output must omit `pinned: true` when unpinned"
        );
    }
}
