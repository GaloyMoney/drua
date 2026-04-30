use chrono::{DateTime, Utc};
use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::primitives::*;

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "ProjectId")]
pub enum ProjectEvent {
    Initialized {
        id: ProjectId,
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
    /// Project declares it sees a `Space` (idempotent on `space_id`).
    SpaceMounted {
        space_id: SpaceId,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Project {
    pub id: ProjectId,
    pub lead_agent_id: AgentId,
    pub name: String,
    #[builder(setter(strip_option), default)]
    pub description: Option<String>,
    #[builder(setter(strip_option), default)]
    pub archived_at: Option<DateTime<Utc>>,
    #[builder(default)]
    pub mounted_spaces: Vec<SpaceId>,
    events: EntityEvents<ProjectEvent>,
}

impl Project {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }

    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }

    pub fn is_space_mounted(&self, space_id: SpaceId) -> bool {
        self.mounted_spaces.contains(&space_id)
    }

    pub(super) fn update(&mut self, description: Option<String>) -> Idempotent<()> {
        if self.description == description {
            return Idempotent::AlreadyApplied;
        }
        self.description = description.clone();
        self.events.push(ProjectEvent::Updated { description });
        Idempotent::Executed(())
    }

    pub(super) fn archive(&mut self) -> Idempotent<()> {
        idempotency_guard!(self.events.iter_all(), already_applied: ProjectEvent::Archived { .. });

        let archived_at = Utc::now();
        self.archived_at = Some(archived_at);
        self.events.push(ProjectEvent::Archived { archived_at });
        Idempotent::Executed(())
    }

    pub(super) fn mount_space(&mut self, space_id: SpaceId) -> Idempotent<()> {
        if self.is_space_mounted(space_id) {
            return Idempotent::AlreadyApplied;
        }
        self.mounted_spaces.push(space_id);
        self.events.push(ProjectEvent::SpaceMounted { space_id });
        Idempotent::Executed(())
    }
}

impl core::fmt::Display for Project {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Project: {}, name: {}", self.id, self.name)
    }
}

impl TryFromEvents<ProjectEvent> for Project {
    fn try_from_events(events: EntityEvents<ProjectEvent>) -> Result<Self, EntityHydrationError> {
        let mut builder = ProjectBuilder::default();
        let mut mounted_spaces: Vec<SpaceId> = Vec::new();

        for event in events.iter_all() {
            match event {
                ProjectEvent::Initialized {
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
                ProjectEvent::Updated { description } => {
                    if let Some(desc) = description {
                        builder = builder.description(desc.clone());
                    }
                }
                ProjectEvent::Archived { archived_at } => {
                    builder = builder.archived_at(*archived_at);
                }
                ProjectEvent::SpaceMounted { space_id } => {
                    if !mounted_spaces.contains(space_id) {
                        mounted_spaces.push(*space_id);
                    }
                }
            }
        }

        builder
            .mounted_spaces(mounted_spaces)
            .events(events)
            .build()
    }
}

#[derive(Debug, Builder)]
pub struct NewProject {
    #[builder(setter(into))]
    pub(super) id: ProjectId,
    #[builder(setter(into))]
    pub(super) lead_agent_id: AgentId,
    #[builder(setter(into))]
    pub(super) name: String,
    #[builder(setter(into, strip_option), default)]
    pub(super) description: Option<String>,
}

impl NewProject {
    pub fn builder() -> NewProjectBuilder {
        let mut builder = NewProjectBuilder::default();
        builder.id(ProjectId::new());
        builder
    }
}

impl IntoEvents<ProjectEvent> for NewProject {
    fn into_events(self) -> EntityEvents<ProjectEvent> {
        EntityEvents::init(
            self.id,
            [ProjectEvent::Initialized {
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

    use crate::primitives::{AgentId, ProjectId};

    use super::{NewProject, Project};

    fn new_project() -> Project {
        let new = NewProject::builder()
            .id(ProjectId::new())
            .lead_agent_id(AgentId::new())
            .name("test-project")
            .description("A test project".to_string())
            .build()
            .unwrap();

        Project::try_from_events(new.into_events()).unwrap()
    }

    #[test]
    fn project_hydration() {
        let project = new_project();
        assert_eq!(project.name, "test-project");
        assert_eq!(project.description, Some("A test project".to_string()));
    }

    #[test]
    fn project_archive() {
        let mut project = new_project();
        assert!(!project.is_archived());
        let _ = project.archive();
        assert!(project.is_archived());
        assert!(project.archived_at.is_some());
    }

    #[test]
    fn project_update_is_idempotent_on_same_description() {
        let mut project = new_project();
        let res = project.update(Some("A test project".to_string()));
        assert!(matches!(res, es_entity::Idempotent::AlreadyApplied));

        let res = project.update(Some("A new description".to_string()));
        assert!(matches!(res, es_entity::Idempotent::Executed(())));
        assert_eq!(project.description.as_deref(), Some("A new description"));

        let res = project.update(Some("A new description".to_string()));
        assert!(matches!(res, es_entity::Idempotent::AlreadyApplied));
    }

    #[test]
    fn project_hydration_without_description() {
        let new = NewProject::builder()
            .id(ProjectId::new())
            .lead_agent_id(AgentId::new())
            .name("minimal")
            .build()
            .unwrap();

        let project = Project::try_from_events(new.into_events()).unwrap();
        assert_eq!(project.name, "minimal");
        assert_eq!(project.description, None);
    }

    #[test]
    fn mount_space_appends_and_is_idempotent() {
        let mut project = new_project();
        assert!(project.mounted_spaces.is_empty());

        let space_id = crate::primitives::SpaceId::new();
        let res = project.mount_space(space_id);
        assert!(res.did_execute());
        assert!(project.is_space_mounted(space_id));
        assert_eq!(project.mounted_spaces, vec![space_id]);

        let again = project.mount_space(space_id);
        assert!(!again.did_execute());
        assert_eq!(project.mounted_spaces, vec![space_id]);
    }

    #[test]
    fn mount_space_hydration_replays_events() {
        let id = ProjectId::new();
        let space_a = crate::primitives::SpaceId::new();
        let space_b = crate::primitives::SpaceId::new();

        let events = es_entity::EntityEvents::init(
            id,
            [
                super::ProjectEvent::Initialized {
                    id,
                    lead_agent_id: AgentId::new(),
                    name: "p".to_string(),
                    description: None,
                },
                super::ProjectEvent::SpaceMounted { space_id: space_a },
                super::ProjectEvent::SpaceMounted { space_id: space_b },
                super::ProjectEvent::SpaceMounted { space_id: space_a },
            ],
        );
        let project = Project::try_from_events(events).unwrap();
        assert_eq!(project.mounted_spaces, vec![space_a, space_b]);
    }
}
