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
        workspace_id: WorkspaceId,
        name: String,
        description: Option<String>,
        status: ProjectStatus,
    },
    Updated {
        name: String,
        description: Option<String>,
    },
    StatusChanged {
        status: ProjectStatus,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Project {
    pub id: ProjectId,
    pub workspace_id: WorkspaceId,
    pub name: String,
    #[builder(setter(strip_option), default)]
    pub description: Option<String>,
    pub status: ProjectStatus,
    events: EntityEvents<ProjectEvent>,
}

impl Project {
    pub fn created_at(&self) -> DateTime<Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }

    pub fn is_archived(&self) -> bool {
        self.status == ProjectStatus::Archived
    }

    pub(super) fn update(&mut self, name: impl Into<String>, description: Option<String>) {
        let name = name.into();
        self.name = name.clone();
        self.description = description.clone();
        self.events
            .push(ProjectEvent::Updated { name, description });
    }

    pub(super) fn change_status(&mut self, status: ProjectStatus) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied: ProjectEvent::StatusChanged { status: s } if *s == status,
            resets_on: ProjectEvent::StatusChanged { .. }
        );

        self.status = status;
        self.events.push(ProjectEvent::StatusChanged { status });
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

        for event in events.iter_all() {
            match event {
                ProjectEvent::Initialized {
                    id,
                    workspace_id,
                    name,
                    description,
                    status,
                } => {
                    builder = builder
                        .id(*id)
                        .workspace_id(*workspace_id)
                        .name(name.clone())
                        .status(*status);
                    if let Some(desc) = description {
                        builder = builder.description(desc.clone());
                    }
                }
                ProjectEvent::Updated { name, description } => {
                    builder = builder.name(name.clone());
                    if let Some(desc) = description {
                        builder = builder.description(desc.clone());
                    }
                }
                ProjectEvent::StatusChanged { status } => {
                    builder = builder.status(*status);
                }
            }
        }

        builder.events(events).build()
    }
}

#[derive(Debug, Builder)]
pub struct NewProject {
    #[builder(setter(into))]
    pub(super) id: ProjectId,
    pub(super) workspace_id: WorkspaceId,
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
                workspace_id: self.workspace_id,
                name: self.name,
                description: self.description,
                status: ProjectStatus::Active,
            }],
        )
    }
}

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};

    use crate::primitives::{ProjectId, ProjectStatus, WorkspaceId};

    use super::{NewProject, Project};

    fn new_project() -> Project {
        let new = NewProject::builder()
            .id(ProjectId::new())
            .workspace_id(WorkspaceId::new())
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
        assert_eq!(project.status, ProjectStatus::Active);
    }

    #[test]
    fn project_update() {
        let mut project = new_project();
        project.update("renamed", Some("new desc".to_string()));
        assert_eq!(project.name, "renamed");
        assert_eq!(project.description, Some("new desc".to_string()));
    }

    #[test]
    fn project_change_status() {
        let mut project = new_project();
        assert_eq!(project.status, ProjectStatus::Active);

        assert!(project
            .change_status(ProjectStatus::Completed)
            .did_execute());
        assert_eq!(project.status, ProjectStatus::Completed);

        // Idempotent: same status again
        assert!(!project
            .change_status(ProjectStatus::Completed)
            .did_execute());

        // Can change again
        assert!(project.change_status(ProjectStatus::Archived).did_execute());
        assert!(project.is_archived());
    }

    #[test]
    fn project_hydration_without_description() {
        let new = NewProject::builder()
            .id(ProjectId::new())
            .workspace_id(WorkspaceId::new())
            .name("minimal")
            .build()
            .unwrap();

        let project = Project::try_from_events(new.into_events()).unwrap();
        assert_eq!(project.name, "minimal");
        assert_eq!(project.description, None);
    }
}
