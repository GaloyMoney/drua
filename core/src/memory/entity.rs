use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::primitives::*;

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "MemoryId")]
pub enum MemoryEvent {
    Initialized {
        id: MemoryId,
        title: String,
        content: String,
        tags: Vec<String>,
    },
    Pinned {},
    Unpinned {},
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Memory {
    pub id: MemoryId,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    #[builder(default)]
    pub pinned: bool,
    events: EntityEvents<MemoryEvent>,
}

impl Memory {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }
}

impl core::fmt::Display for Memory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Memory: {}, title: {}", self.id, self.title)
    }
}

impl TryFromEvents<MemoryEvent> for Memory {
    fn try_from_events(events: EntityEvents<MemoryEvent>) -> Result<Self, EntityHydrationError> {
        let mut builder = MemoryBuilder::default();

        for event in events.iter_all() {
            match event {
                MemoryEvent::Initialized {
                    id,
                    title,
                    content,
                    tags,
                } => {
                    builder = builder
                        .id(*id)
                        .title(title.clone())
                        .content(content.clone())
                        .tags(tags.clone());
                }
                MemoryEvent::Pinned {} => {
                    builder = builder.pinned(true);
                }
                MemoryEvent::Unpinned {} => {
                    builder = builder.pinned(false);
                }
            }
        }

        builder.events(events).build()
    }
}

#[derive(Debug, Builder)]
pub struct NewMemory {
    #[builder(setter(into))]
    pub(super) id: MemoryId,
    #[builder(setter(into))]
    pub(super) title: String,
    #[builder(setter(into))]
    pub(super) content: String,
    pub(super) tags: Vec<String>,
}

impl NewMemory {
    pub fn builder() -> NewMemoryBuilder {
        let mut builder = NewMemoryBuilder::default();
        builder.id(MemoryId::new());
        builder
    }
}

impl IntoEvents<MemoryEvent> for NewMemory {
    fn into_events(self) -> EntityEvents<MemoryEvent> {
        EntityEvents::init(
            self.id,
            [MemoryEvent::Initialized {
                id: self.id,
                title: self.title,
                content: self.content,
                tags: self.tags,
            }],
        )
    }
}

/// A search result with relevance scoring and decay information.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub id: MemoryId,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub score: f64,
    /// Decay factor (1.0 = fully fresh, 0.0 = fully decayed).
    /// Pinned memories always have 1.0.
    pub decay_factor: f64,
    pub pinned: bool,
}

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};

    use crate::primitives::MemoryId;

    use super::{Memory, NewMemory};

    fn new_memory() -> Memory {
        let new = NewMemory::builder()
            .id(MemoryId::new())
            .title("Test finding")
            .content("Some important content")
            .tags(vec!["test".to_string()])
            .build()
            .unwrap();

        Memory::try_from_events(new.into_events()).unwrap()
    }

    #[test]
    fn memory_hydration() {
        let memory = new_memory();
        assert_eq!(memory.title, "Test finding");
        assert_eq!(memory.content, "Some important content");
        assert_eq!(memory.tags, vec!["test"]);
        assert!(!memory.pinned);
    }

    #[test]
    fn memory_hydration_minimal() {
        let new = NewMemory::builder()
            .id(MemoryId::new())
            .title("Minimal memory")
            .content("content")
            .tags(vec![])
            .build()
            .unwrap();

        let memory = Memory::try_from_events(new.into_events()).unwrap();
        assert_eq!(memory.title, "Minimal memory");
        assert!(!memory.pinned);
    }
}
