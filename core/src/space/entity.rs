use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::primitives::*;

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "SpaceId")]
pub enum SpaceEvent {
    Initialized {
        id: SpaceId,
        slug: String,
        description: Option<String>,
        /// Workspaces seeded with access at creation time. Always
        /// non-empty: the creating agent's workspace is auto-added.
        authorized_workspaces: Vec<WorkspaceId>,
    },
    /// Adds a workspace to `authorized_workspaces` (idempotent).
    WorkspaceAuthorized { workspace_id: WorkspaceId },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Space {
    pub id: SpaceId,
    pub slug: String,
    #[builder(setter(strip_option), default)]
    pub description: Option<String>,
    pub authorized_workspaces: Vec<WorkspaceId>,
    events: EntityEvents<SpaceEvent>,
}

impl Space {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }

    pub fn is_workspace_authorized(&self, workspace_id: WorkspaceId) -> bool {
        self.authorized_workspaces.contains(&workspace_id)
    }

    /// Used by future `spaces.authorize` MCP command; the
    /// `WorkspaceAuthorized` event is already part of the wire schema so
    /// emitting it later is non-breaking.
    #[allow(dead_code)]
    pub(super) fn authorize_workspace(&mut self, workspace_id: WorkspaceId) -> Idempotent<()> {
        if self.is_workspace_authorized(workspace_id) {
            return Idempotent::AlreadyApplied;
        }
        self.authorized_workspaces.push(workspace_id);
        self.events
            .push(SpaceEvent::WorkspaceAuthorized { workspace_id });
        Idempotent::Executed(())
    }
}

impl core::fmt::Display for Space {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Space: {}, slug: {}", self.id, self.slug)
    }
}

impl TryFromEvents<SpaceEvent> for Space {
    fn try_from_events(events: EntityEvents<SpaceEvent>) -> Result<Self, EntityHydrationError> {
        let mut builder = SpaceBuilder::default();
        let mut authorized_workspaces: Vec<WorkspaceId> = Vec::new();

        for event in events.iter_all() {
            match event {
                SpaceEvent::Initialized {
                    id,
                    slug,
                    description,
                    authorized_workspaces: initial,
                } => {
                    builder = builder.id(*id).slug(slug.clone());
                    if let Some(desc) = description {
                        builder = builder.description(desc.clone());
                    }
                    authorized_workspaces.extend(initial.iter().copied());
                }
                SpaceEvent::WorkspaceAuthorized { workspace_id } => {
                    if !authorized_workspaces.contains(workspace_id) {
                        authorized_workspaces.push(*workspace_id);
                    }
                }
            }
        }

        builder
            .authorized_workspaces(authorized_workspaces)
            .events(events)
            .build()
    }
}

#[derive(Debug, Builder)]
pub struct NewSpace {
    #[builder(setter(into))]
    pub(super) id: SpaceId,
    #[builder(setter(into))]
    pub(super) slug: String,
    #[builder(setter(into, strip_option), default)]
    pub(super) description: Option<String>,
    /// Seeded into `authorized_workspaces` on the `Initialized` event.
    /// Empty by default; callers should add the creating workspace.
    #[builder(default)]
    pub(super) authorized_workspaces: Vec<WorkspaceId>,
}

impl NewSpace {
    pub fn builder() -> NewSpaceBuilder {
        let mut builder = NewSpaceBuilder::default();
        builder.id(SpaceId::new());
        builder
    }
}

impl IntoEvents<SpaceEvent> for NewSpace {
    fn into_events(self) -> EntityEvents<SpaceEvent> {
        EntityEvents::init(
            self.id,
            [SpaceEvent::Initialized {
                id: self.id,
                slug: self.slug,
                description: self.description,
                authorized_workspaces: self.authorized_workspaces,
            }],
        )
    }
}

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};

    use super::{NewSpace, Space};
    use crate::primitives::{SpaceId, WorkspaceId};

    fn new_space(workspaces: Vec<WorkspaceId>) -> Space {
        let new = NewSpace::builder()
            .id(SpaceId::new())
            .slug("oncall")
            .description("On-call rotation".to_string())
            .authorized_workspaces(workspaces)
            .build()
            .unwrap();
        Space::try_from_events(new.into_events()).unwrap()
    }

    #[test]
    fn space_hydration_with_initial_workspace() {
        let ws = WorkspaceId::new();
        let s = new_space(vec![ws]);
        assert_eq!(s.slug, "oncall");
        assert_eq!(s.description.as_deref(), Some("On-call rotation"));
        assert_eq!(s.authorized_workspaces, vec![ws]);
        assert!(s.is_workspace_authorized(ws));
    }

    #[test]
    fn space_hydration_without_workspaces() {
        let new = NewSpace::builder()
            .id(SpaceId::new())
            .slug("incidents")
            .build()
            .unwrap();
        let s = Space::try_from_events(new.into_events()).unwrap();
        assert_eq!(s.slug, "incidents");
        assert_eq!(s.authorized_workspaces, Vec::<WorkspaceId>::new());
    }

    #[test]
    fn authorize_workspace_appends_and_is_idempotent() {
        let creator = WorkspaceId::new();
        let mut s = new_space(vec![creator]);

        let other = WorkspaceId::new();
        let res = s.authorize_workspace(other);
        assert!(res.did_execute());
        assert_eq!(s.authorized_workspaces, vec![creator, other]);

        let again = s.authorize_workspace(other);
        assert!(!again.did_execute());
        assert_eq!(s.authorized_workspaces, vec![creator, other]);
    }

    #[test]
    fn authorize_workspace_idempotent_for_creator() {
        let creator = WorkspaceId::new();
        let mut s = new_space(vec![creator]);
        let res = s.authorize_workspace(creator);
        assert!(!res.did_execute());
    }

    #[test]
    fn hydration_replays_authorize_events() {
        let id = SpaceId::new();
        let creator = WorkspaceId::new();
        let other = WorkspaceId::new();

        let events = es_entity::EntityEvents::init(
            id,
            [
                super::SpaceEvent::Initialized {
                    id,
                    slug: "oncall".to_string(),
                    description: None,
                    authorized_workspaces: vec![creator],
                },
                super::SpaceEvent::WorkspaceAuthorized {
                    workspace_id: other,
                },
            ],
        );
        let s = Space::try_from_events(events).unwrap();
        assert_eq!(s.authorized_workspaces, vec![creator, other]);
    }
}
