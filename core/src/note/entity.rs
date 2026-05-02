use derive_builder::Builder;
use drua_library::{GitFileHash, SearchableFields, WriteOp};
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::note::file::{canonical_note_path, render_note_markdown};
use crate::note::NOTE_DOC_TYPE;
use crate::primitives::*;
use crate::skill::file::slugify;

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "NoteId")]
pub enum NoteEvent {
    Initialized {
        id: NoteId,
        project_id: ProjectId,
        project_name: String,
        title: String,
        content: String,
        tags: Vec<String>,
        file_hash: GitFileHash,
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
    pub(crate) project_id: ProjectId,
    pub(crate) project_name: String,
    pub(crate) title: String,
    pub(crate) content: String,
    pub(crate) tags: Vec<String>,
    pub(crate) file_hash: Option<GitFileHash>,
    pub(crate) pinned: bool,
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

    /// Whether the note is pinned (injected into the project's
    /// shared agent context).
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
        SearchableFields {
            doc_id: self.id.into(),
            doc_type: NOTE_DOC_TYPE,
            scope_id: Some(self.project_id.into()),
            scope_slug: Some(self.project_name.clone()),
            name: self.title.clone(),
            path: Some(canonical_note_path(
                self.id,
                &self.title,
                &self.project_name,
            )),
            content: self.content.clone(),
        }
    }

    fn write_op(&self) -> WriteOp {
        let canonical = canonical_note_path(self.id, &self.title, &self.project_name);
        let content = self.rendered().into_bytes();
        let id_uuid: uuid::Uuid = self.id.into();
        let message = format!(
            "note: {}-{}",
            slugify(&self.title),
            &id_uuid.to_string()[..8]
        );
        WriteOp::WriteFile {
            path: canonical,
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
                    title,
                    content,
                    tags,
                    file_hash,
                } => {
                    builder = builder
                        .id(*id)
                        .project_id(*project_id)
                        .project_name(project_name.clone())
                        .title(title.clone())
                        .content(content.clone())
                        .tags(tags.clone())
                        .file_hash(Some(file_hash.clone()))
                        .pinned(false);
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
pub struct NewNote {
    #[builder(setter(into))]
    pub(super) id: NoteId,
    #[builder(setter(into))]
    pub(super) project_id: ProjectId,
    #[builder(setter(into))]
    pub(super) project_name: String,
    #[builder(setter(into))]
    pub(super) title: String,
    #[builder(setter(into))]
    pub(super) content: String,
    #[builder(default)]
    pub(super) tags: Vec<String>,
    pub(super) file_hash: GitFileHash,
    #[builder(default)]
    pub(super) pinned: bool,
}

impl NewNote {
    pub fn builder() -> NewNoteBuilder {
        let mut builder = NewNoteBuilder::default();
        builder.id(NoteId::new());
        builder
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
                title: self.title,
                content: self.content,
                tags: self.tags,
                file_hash: self.file_hash,
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
            .project_name("test")
            .title("Test Note")
            .content("Some content here")
            .tags(vec!["tag1".into(), "tag2".into()])
            .file_hash(test_hash())
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
        assert_eq!(note.project_name, "test");
        assert!(note.file_hash.is_some());
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
}
