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
    Archived {},
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Note {
    pub id: NoteId,
    pub workspace_id: WorkspaceId,
    pub title: String,
    pub content: String,
    #[builder(default)]
    pub tags: Vec<String>,
    #[builder(default)]
    pub file_hash: Option<GitFileHash>,
    #[builder(default)]
    pub archived: bool,
    pub(super) events: EntityEvents<NoteEvent>,
}

impl Note {
    pub fn as_runtime_file(
        &self,
        workspace_name: &str,
    ) -> crate::library::RuntimeFile {
        let created_at = self
            .events
            .entity_first_persisted_at()
            .map(|t| t.to_rfc3339())
            .unwrap_or_default();

        crate::library::RuntimeFile::for_note(
            self.id,
            self.workspace_id,
            workspace_name,
            &self.title,
            &self.content,
            &self.tags,
            &created_at,
        )
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

    pub fn archive(&mut self) -> Idempotent<()> {
        idempotency_guard!(self.events.iter_all(), already_applied: NoteEvent::Archived { .. });

        self.archived = true;
        self.events.push(NoteEvent::Archived {});
        Idempotent::Executed(())
    }
}

impl std::fmt::Display for Note {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "id: {}\ntitle: {}", self.id, self.title)?;
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
                    title,
                    content,
                    tags,
                    file_hash,
                } => {
                    builder = builder
                        .id(*id)
                        .workspace_id(*workspace_id)
                        .title(title.clone())
                        .content(content.clone())
                        .tags(tags.clone())
                        .file_hash(Some(file_hash.clone()));
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
                NoteEvent::Archived {} => {
                    builder = builder.archived(true);
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
    pub(super) title: String,
    #[builder(setter(into))]
    pub(super) content: String,
    #[builder(default)]
    pub(super) tags: Vec<String>,
    pub(super) file_hash: GitFileHash,
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
        // Use RuntimeFile machinery to compute a real hash
        let rf = crate::library::RuntimeFile::for_note(
            NoteId::new(),
            WorkspaceId::new(),
            "test",
            "Test Note",
            "Some content here",
            &["tag1".into(), "tag2".into()],
            "",
        );
        rf.file_hash()
    }

    fn new_note() -> Note {
        let id = NoteId::new();
        let new = NewNote::builder()
            .id(id)
            .workspace_id(WorkspaceId::new())
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
        assert!(!note.archived);
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
    fn note_archive() {
        let mut note = new_note();
        assert!(!note.archived);
        let _ = note.archive();
        assert!(note.archived);
    }
}
