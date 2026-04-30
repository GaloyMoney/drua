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
    /// Document content with frontmatter, as stored in the library.
    pub fn content(&self) -> String {
        self.as_runtime_file().content()
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

    /// Whether the note is pinned (injected into the workspace's
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

    pub(super) fn as_runtime_file(&self) -> crate::library::UpstreamOp {
        crate::library::UpstreamOp::WriteFile(Box::new(
            <Self as crate::library::LibrarySynced>::to_synced_file(self),
        ))
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

impl crate::library::LibrarySynced for Note {
    type Event = NoteEvent;
    const DOC_TYPE: crate::library::DocType = crate::library::DocType::Note;

    fn is_content_event(ev: &NoteEvent) -> bool {
        matches!(
            ev,
            NoteEvent::Initialized { .. } | NoteEvent::Updated { .. }
        )
    }

    fn workspace(&self) -> Option<(WorkspaceId, &str)> {
        Some((self.workspace_id, &self.workspace_name))
    }

    fn id(&self) -> uuid::Uuid {
        self.id.into()
    }

    fn display_name(&self) -> &str {
        &self.title
    }

    fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .unwrap_or_else(chrono::Utc::now)
    }

    fn updated_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_last_modified_at()
            .or_else(|| self.events.entity_first_persisted_at())
            .unwrap_or_else(chrono::Utc::now)
    }

    fn index_body(&self) -> &str {
        &self.content
    }

    fn index_tags(&self) -> Vec<String> {
        self.tags.clone()
    }

    fn render(&self) -> String {
        crate::library::render_note_markdown(
            self.id.into(),
            &self.title,
            &self.content,
            &self.tags,
            &<Self as crate::library::LibrarySynced>::created_at(self).to_rfc3339(),
            &<Self as crate::library::LibrarySynced>::updated_at(self).to_rfc3339(),
        )
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
        let rf = crate::library::UpstreamOp::for_note(
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
