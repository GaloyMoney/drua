use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::library::GitFileHash;
use crate::primitives::*;

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "NoteId")]
pub enum NoteEvent {
    Initialized {
        id: NoteId,
        workspace_id: WorkspaceId,
        workspace_name: String,
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
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) workspace_name: String,
    pub(crate) title: String,
    pub(crate) content: String,
    pub(crate) tags: Vec<String>,
    pub(crate) file_hash: Option<GitFileHash>,
    pub(crate) pinned: bool,
    pub(super) events: EntityEvents<NoteEvent>,
}

impl Note {
    /// Full document content (with frontmatter) as stored in the library.
    pub fn content(&self) -> String {
        self.as_runtime_file().content()
    }

    pub(crate) fn created_at(&self) -> String {
        self.events
            .entity_first_persisted_at()
            .map(|t| t.to_rfc3339())
            .unwrap_or_default()
    }

    pub(super) fn as_runtime_file(&self) -> crate::library::RuntimeFile {
        let created_at = self.created_at();
        let updated_at = self
            .events
            .entity_last_modified_at()
            .map(|t| t.to_rfc3339())
            .unwrap_or_default();

        crate::library::RuntimeFile::for_note(
            self.id,
            self.workspace_id,
            &self.workspace_name,
            &self.title,
            &self.content,
            &self.tags,
            &created_at,
            &updated_at,
        )
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
    ) {
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
    }
}

impl From<Note> for crate::library::SearchResult {
    fn from(n: Note) -> Self {
        Self {
            doc_id: uuid::Uuid::from(n.id),
            doc_type: crate::library::DocType::Note,
            title: n.title,
            content: n.content,
            tags: n.tags,
            score: 0.0,
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
                    workspace_id,
                    workspace_name,
                    title,
                    content,
                    tags,
                    file_hash,
                } => {
                    builder = builder
                        .id(*id)
                        .workspace_id(*workspace_id)
                        .workspace_name(workspace_name.clone())
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
    pub(super) workspace_id: WorkspaceId,
    #[builder(setter(into))]
    pub(super) workspace_name: String,
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
                workspace_id: self.workspace_id,
                workspace_name: self.workspace_name,
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

    use crate::library::GitFileHash;
    use crate::primitives::{NoteId, WorkspaceId};

    use super::{NewNote, Note};

    fn test_hash() -> GitFileHash {
        let rf = crate::library::RuntimeFile::for_note(
            NoteId::new(),
            WorkspaceId::new(),
            "test",
            "Test Note",
            "Some content here",
            &["tag1".into(), "tag2".into()],
            "",
            "",
        );
        rf.file_hash()
    }

    fn new_note() -> Note {
        let id = NoteId::new();
        let new = NewNote::builder()
            .id(id)
            .workspace_id(WorkspaceId::new())
            .workspace_name("test")
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
        assert_eq!(note.workspace_name, "test");
        assert!(note.file_hash.is_some());
    }

    #[test]
    fn note_update() {
        let mut note = new_note();
        note.update(
            "Updated Title".into(),
            "Updated content".into(),
            vec!["new-tag".into()],
            test_hash(),
        );
        assert_eq!(note.title, "Updated Title");
        assert_eq!(note.content, "Updated content");
        assert_eq!(note.tags, vec!["new-tag"]);
    }

    #[test]
    fn note_pin_unpin() {
        let mut note = new_note();
        assert!(!note.pinned);

        // Pin
        let result = note.pin();
        assert!(matches!(result, es_entity::Idempotent::Executed(())));
        assert!(note.pinned);

        // Pin again is idempotent
        let result = note.pin();
        assert!(matches!(result, es_entity::Idempotent::AlreadyApplied));
        assert!(note.pinned);

        // Unpin
        let result = note.unpin();
        assert!(matches!(result, es_entity::Idempotent::Executed(())));
        assert!(!note.pinned);

        // Unpin again is idempotent
        let result = note.unpin();
        assert!(matches!(result, es_entity::Idempotent::AlreadyApplied));
        assert!(!note.pinned);
    }
}
