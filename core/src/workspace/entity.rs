use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::primitives::*;

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "WorkspaceId")]
pub enum WorkspaceEvent {
    Initialized {
        id: WorkspaceId,
        name: String,
        description: Option<String>,
    },
    Updated {
        name: String,
        description: Option<String>,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    #[builder(setter(strip_option), default)]
    pub description: Option<String>,
    events: EntityEvents<WorkspaceEvent>,
}

impl Workspace {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }

    pub(super) fn update(&mut self, name: impl Into<String>, description: Option<String>) {
        let name = name.into();
        self.name = name.clone();
        self.description = description.clone();
        self.events
            .push(WorkspaceEvent::Updated { name, description });
    }
}

impl core::fmt::Display for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Workspace: {}, name: {}", self.id, self.name)
    }
}

impl TryFromEvents<WorkspaceEvent> for Workspace {
    fn try_from_events(events: EntityEvents<WorkspaceEvent>) -> Result<Self, EntityHydrationError> {
        let mut builder = WorkspaceBuilder::default();

        for event in events.iter_all() {
            match event {
                WorkspaceEvent::Initialized {
                    id,
                    name,
                    description,
                } => {
                    builder = builder.id(*id).name(name.clone());
                    if let Some(desc) = description {
                        builder = builder.description(desc.clone());
                    }
                }
                WorkspaceEvent::Updated { name, description } => {
                    builder = builder.name(name.clone());
                    if let Some(desc) = description {
                        builder = builder.description(desc.clone());
                    }
                }
            }
        }

        builder.events(events).build()
    }
}

#[derive(Debug, Builder)]
pub struct NewWorkspace {
    #[builder(setter(into))]
    pub(super) id: WorkspaceId,
    #[builder(setter(into))]
    pub(super) name: String,
    #[builder(setter(into, strip_option), default)]
    pub(super) description: Option<String>,
}

impl NewWorkspace {
    pub fn builder() -> NewWorkspaceBuilder {
        let mut builder = NewWorkspaceBuilder::default();
        builder.id(WorkspaceId::new());
        builder
    }
}

impl IntoEvents<WorkspaceEvent> for NewWorkspace {
    fn into_events(self) -> EntityEvents<WorkspaceEvent> {
        EntityEvents::init(
            self.id,
            [WorkspaceEvent::Initialized {
                id: self.id,
                name: self.name,
                description: self.description,
            }],
        )
    }
}

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};

    use crate::primitives::WorkspaceId;

    use super::{NewWorkspace, Workspace};

    fn new_workspace() -> Workspace {
        let new = NewWorkspace::builder()
            .id(WorkspaceId::new())
            .name("test-workspace")
            .description("A test workspace".to_string())
            .build()
            .unwrap();

        Workspace::try_from_events(new.into_events()).unwrap()
    }

    #[test]
    fn workspace_hydration() {
        let ws = new_workspace();
        assert_eq!(ws.name, "test-workspace");
        assert_eq!(ws.description, Some("A test workspace".to_string()));
    }

    #[test]
    fn workspace_hydration_without_description() {
        let new = NewWorkspace::builder()
            .id(WorkspaceId::new())
            .name("minimal")
            .build()
            .unwrap();

        let ws = Workspace::try_from_events(new.into_events()).unwrap();
        assert_eq!(ws.name, "minimal");
        assert_eq!(ws.description, None);
    }
}
