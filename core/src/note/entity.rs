use derive_builder::Builder;
use drua_library::{GitFileHash, SearchableFields, WriteOp};
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::note::file::render_note_markdown;
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
        file_hash: GitFileHash,
        /// Repo-relative on-disk path. Sacred — never mutated by the
        /// importer. Pre-path-identity events deserialise with `""`;
        /// hydration falls back to a derived value.
        #[serde(default)]
        path: String,
        /// Initial pinned state. Set from frontmatter (`pinned: true`)
        /// when a space note is imported; defaults to `false` for
        /// project notes and pre-pinned-tier events.
        #[serde(default)]
        pinned: bool,
    },
    Updated {
        title: String,
        content: String,
        tags: Vec<String>,
        file_hash: GitFileHash,
    },
    Pinned {},
    Unpinned {},
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
    pub(crate) file_hash: Option<GitFileHash>,
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

    /// Canonical on-disk content (markdown w/ frontmatter).
    pub(super) fn rendered(&self) -> String {
        render_note_markdown(
            self.id.into(),
            &self.title,
            &self.content,
            &self.tags,
            &self.created_at(),
            &self.updated_at_rfc3339(),
        )
    }

    fn updated_at_rfc3339(&self) -> String {
        self.events
            .entity_last_modified_at()
            .or_else(|| self.events.entity_first_persisted_at())
            .map(|t| t.to_rfc3339())
            .unwrap_or_default()
    }

    pub fn pin(&mut self) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied: NoteEvent::Pinned {},
            resets_on: NoteEvent::Unpinned { .. }
        );
        self.pinned = true;
        self.events.push(NoteEvent::Pinned {});
        Idempotent::Executed(())
    }

    pub fn unpin(&mut self) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied: NoteEvent::Unpinned {},
            resets_on: NoteEvent::Pinned { .. }
        );
        self.pinned = false;
        self.events.push(NoteEvent::Unpinned {});
        Idempotent::Executed(())
    }

    pub fn update(
        &mut self,
        title: String,
        content: String,
        tags: Vec<String>,
        file_hash: GitFileHash,
    ) -> Idempotent<()> {
        if self.file_hash.as_ref() == Some(&file_hash) {
            return Idempotent::AlreadyApplied;
        }
        self.title = title.clone();
        self.content = content.clone();
        self.tags = tags.clone();
        self.file_hash = Some(file_hash.clone());
        self.events.push(NoteEvent::Updated {
            title,
            content,
            tags,
            file_hash,
        });
        Idempotent::Executed(())
    }
}

impl drua_library::LibrarySynced for Note {
    type Event = NoteEvent;

    fn is_content_event(ev: &NoteEvent) -> bool {
        matches!(
            ev,
            NoteEvent::Initialized { .. } | NoteEvent::Updated { .. }
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
                    file_hash,
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
                        .file_hash(Some(file_hash.clone()))
                        .pinned(*pinned)
                        .path(path.clone());
                }
                NoteEvent::Updated {
                    title,
                    content,
                    tags,
                    file_hash,
                } => {
                    builder = builder
                        .title(title.clone())
                        .content(content.clone())
                        .tags(tags.clone())
                        .file_hash(Some(file_hash.clone()));
                }
                NoteEvent::Pinned {} => {
                    builder = builder.pinned(true);
                }
                NoteEvent::Unpinned {} => {
                    builder = builder.pinned(false);
                }
            }
        }

        builder.events(events).build()
    }
}

#[derive(Debug, Builder)]
#[builder(pattern = "owned")]
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
    pub(super) file_hash: GitFileHash,
    #[builder(default)]
    pub(super) pinned: bool,
    /// Repo-relative on-disk path. Required — there's no canonicalisation
    /// fallback. `Notes::store` derives it from `(project_name, title)`;
    /// the importer passes through whatever the file's real path is.
    #[builder(setter(into))]
    pub(super) path: String,
}

impl NewNote {
    pub fn builder() -> NewNoteBuilder {
        NewNoteBuilder::default().id(NoteId::new())
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
                file_hash: self.file_hash,
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

    use super::{NewNote, Note, NoteEvent};

    fn test_hash() -> GitFileHash {
        let id = NoteId::new();
        let rendered = super::render_note_markdown(
            id.into(),
            "Test Note",
            "Some content here",
            &["tag1".into(), "tag2".into()],
            "",
            "",
        );
        GitFileHash::new(rendered)
    }

    fn new_note() -> Note {
        let id = NoteId::new();
        let new = NewNote::builder()
            .id(id)
            .project_id(ProjectId::new())
            .project_name("test".to_string())
            .title("Test Note")
            .content("Some content here")
            .tags(vec!["tag1".into(), "tag2".into()])
            .file_hash(test_hash())
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
        assert!(note.file_hash.is_some());
        assert_eq!(note.path, "runtime/projects/test/notes/test-note.md");
    }

    #[test]
    fn note_update() {
        let mut note = new_note();
        let res = note.update(
            "Updated Title".into(),
            "Updated content".into(),
            vec!["new-tag".into()],
            test_hash(),
        );
        assert!(matches!(res, es_entity::Idempotent::Executed(())));
        assert_eq!(note.title, "Updated Title");
        assert_eq!(note.content, "Updated content");
        assert_eq!(note.tags, vec!["new-tag"]);
    }

    #[test]
    fn note_update_is_idempotent_on_same_file_hash() {
        let mut note = new_note();
        let hash = test_hash();
        let _ = note.update("T".into(), "C".into(), vec!["t".into()], hash.clone());
        let res = note.update("T2".into(), "C2".into(), vec!["t2".into()], hash);
        assert!(matches!(res, es_entity::Idempotent::AlreadyApplied));
        assert_eq!(note.title, "T", "second update must not mutate");
    }

    #[test]
    fn note_pin_unpin() {
        let mut note = new_note();
        assert!(!note.pinned);

        let result = note.pin();
        assert!(matches!(result, es_entity::Idempotent::Executed(())));
        assert!(note.pinned);

        let result = note.pin();
        assert!(matches!(result, es_entity::Idempotent::AlreadyApplied));
        assert!(note.pinned);

        let result = note.unpin();
        assert!(matches!(result, es_entity::Idempotent::Executed(())));
        assert!(!note.pinned);

        let result = note.unpin();
        assert!(matches!(result, es_entity::Idempotent::AlreadyApplied));
        assert!(!note.pinned);
    }

    #[test]
    fn legacy_initialized_event_without_path_deserializes() {
        // Pre-path-identity events have no `path` key. `#[serde(default)]`
        // hydrates as empty string.
        let json = serde_json::json!({
            "type": "initialized",
            "id": uuid::Uuid::new_v4(),
            "project_id": uuid::Uuid::new_v4(),
            "project_name": "proj",
            "title": "t",
            "content": "c",
            "tags": [],
            "file_hash": "abc",
        });
        let ev: NoteEvent = serde_json::from_value(json).expect("legacy event");
        match ev {
            NoteEvent::Initialized {
                space_id,
                space_slug,
                path,
                pinned,
                ..
            } => {
                assert!(space_id.is_none());
                assert!(space_slug.is_none());
                assert!(path.is_empty(), "legacy events default path to empty");
                assert!(!pinned, "legacy events default pinned to false");
            }
            _ => panic!("expected Initialized"),
        }
    }
}
