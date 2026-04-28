use chrono::{DateTime, Utc};
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
        lead_agent_id: AgentId,
        name: String,
        description: Option<String>,
    },
    Updated {
        description: Option<String>,
    },
    Archived {
        archived_at: DateTime<Utc>,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Workspace {
    pub id: WorkspaceId,
    pub lead_agent_id: AgentId,
    pub name: String,
    #[builder(setter(strip_option), default)]
    pub description: Option<String>,
    #[builder(setter(strip_option), default)]
    pub archived_at: Option<DateTime<Utc>>,
    events: EntityEvents<WorkspaceEvent>,
}

impl Workspace {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }

    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }

    pub(super) fn update(&mut self, description: Option<String>) -> Idempotent<()> {
        if self.description == description {
            return Idempotent::AlreadyApplied;
        }
        self.description = description.clone();
        self.events.push(WorkspaceEvent::Updated { description });
        Idempotent::Executed(())
    }

    pub(super) fn archive(&mut self) -> Idempotent<()> {
        idempotency_guard!(self.events.iter_all(), already_applied: WorkspaceEvent::Archived { .. });

        let archived_at = Utc::now();
        self.archived_at = Some(archived_at);
        self.events.push(WorkspaceEvent::Archived { archived_at });
        Idempotent::Executed(())
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
                    lead_agent_id,
                    name,
                    description,
                } => {
                    builder = builder
                        .id(*id)
                        .lead_agent_id(*lead_agent_id)
                        .name(name.clone());
                    if let Some(desc) = description {
                        builder = builder.description(desc.clone());
                    }
                }
                WorkspaceEvent::Updated { description } => {
                    if let Some(desc) = description {
                        builder = builder.description(desc.clone());
                    }
                }
                WorkspaceEvent::Archived { archived_at } => {
                    builder = builder.archived_at(*archived_at);
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
    pub(super) lead_agent_id: AgentId,
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
                lead_agent_id: self.lead_agent_id,
                name: self.name,
                description: self.description,
            }],
        )
    }
}

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};

    use crate::primitives::{AgentId, WorkspaceId};

    use super::{NewWorkspace, Workspace};

    fn new_workspace() -> Workspace {
        let new = NewWorkspace::builder()
            .id(WorkspaceId::new())
            .lead_agent_id(AgentId::new())
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
    fn workspace_archive() {
        let mut ws = new_workspace();
        assert!(!ws.is_archived());
        let _ = ws.archive();
        assert!(ws.is_archived());
        assert!(ws.archived_at.is_some());
    }

    #[test]
    fn workspace_update_is_idempotent_on_same_description() {
        let mut ws = new_workspace();
        let res = ws.update(Some("A test workspace".to_string()));
        assert!(matches!(res, es_entity::Idempotent::AlreadyApplied));

        let res = ws.update(Some("A new description".to_string()));
        assert!(matches!(res, es_entity::Idempotent::Executed(())));
        assert_eq!(ws.description.as_deref(), Some("A new description"));

        let res = ws.update(Some("A new description".to_string()));
        assert!(matches!(res, es_entity::Idempotent::AlreadyApplied));
    }

    #[test]
    fn workspace_hydration_without_description() {
        let new = NewWorkspace::builder()
            .id(WorkspaceId::new())
            .lead_agent_id(AgentId::new())
            .name("minimal")
            .build()
            .unwrap();

        let ws = Workspace::try_from_events(new.into_events()).unwrap();
        assert_eq!(ws.name, "minimal");
        assert_eq!(ws.description, None);
    }
}
