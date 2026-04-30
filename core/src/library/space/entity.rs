use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use super::error::SpaceError;
use crate::primitives::*;

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "SpaceId")]
pub enum SpaceEvent {
    Initialized {
        id: SpaceId,
        slug: String,
        description: Option<String>,
        /// Seeded with `sub.project_id()` when the creator is an
        /// agent; empty when created by a `User` subject. Mutated later
        /// via `ProjectAuthorized`.
        authorized_projects: Vec<ProjectId>,
    },
    /// Adds a project to `authorized_projects` (idempotent).
    ProjectAuthorized { project_id: ProjectId },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Space {
    pub id: SpaceId,
    pub slug: String,
    #[builder(setter(strip_option), default)]
    pub description: Option<String>,
    pub authorized_projects: Vec<ProjectId>,
    events: EntityEvents<SpaceEvent>,
}

impl Space {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }

    pub fn is_project_authorized(&self, project_id: ProjectId) -> bool {
        self.authorized_projects.contains(&project_id)
    }

    /// Used by future `spaces.authorize` MCP command; the
    /// `ProjectAuthorized` event is already part of the wire schema so
    /// emitting it later is non-breaking.
    #[allow(dead_code)]
    pub(super) fn authorize_project(&mut self, project_id: ProjectId) -> Idempotent<()> {
        if self.is_project_authorized(project_id) {
            return Idempotent::AlreadyApplied;
        }
        self.authorized_projects.push(project_id);
        self.events
            .push(SpaceEvent::ProjectAuthorized { project_id });
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
        let mut authorized_projects: Vec<ProjectId> = Vec::new();

        for event in events.iter_all() {
            match event {
                SpaceEvent::Initialized {
                    id,
                    slug,
                    description,
                    authorized_projects: initial,
                } => {
                    builder = builder.id(*id).slug(slug.clone());
                    if let Some(desc) = description {
                        builder = builder.description(desc.clone());
                    }
                    authorized_projects.extend(initial.iter().copied());
                }
                SpaceEvent::ProjectAuthorized { project_id } => {
                    if !authorized_projects.contains(project_id) {
                        authorized_projects.push(*project_id);
                    }
                }
            }
        }

        builder
            .authorized_projects(authorized_projects)
            .events(events)
            .build()
    }
}

#[derive(Debug, Builder)]
#[builder(
    pattern = "owned",
    build_fn(error = "SpaceError", validate = "Self::validate")
)]
pub struct NewSpace {
    #[builder(setter(into))]
    pub(super) id: SpaceId,
    #[builder(setter(into))]
    pub(super) slug: String,
    #[builder(setter(into, strip_option), default)]
    pub(super) description: Option<String>,
    /// Seeded into `authorized_projects` on the `Initialized` event.
    /// Empty by default; callers should add the creating project.
    #[builder(default)]
    pub(super) authorized_projects: Vec<ProjectId>,
}

impl NewSpaceBuilder {
    fn validate(&self) -> Result<(), SpaceError> {
        if let Some(slug) = self.slug.as_ref() {
            validate_slug(slug)?;
        }
        Ok(())
    }
}

impl NewSpace {
    pub fn builder() -> NewSpaceBuilder {
        NewSpaceBuilder::default().id(SpaceId::new())
    }
}

/// Slugs must be lowercase alphanumeric + `-`, with no leading/trailing
/// `-` and no empty segments. This keeps `spaces/<slug>/` safe for
/// filesystem and git refspecs.
fn validate_slug(slug: &str) -> Result<(), SpaceError> {
    if slug.is_empty() || slug.starts_with('-') || slug.ends_with('-') || slug.contains("--") {
        return Err(SpaceError::InvalidSlug {
            slug: slug.to_string(),
        });
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(SpaceError::InvalidSlug {
            slug: slug.to_string(),
        });
    }
    Ok(())
}

impl IntoEvents<SpaceEvent> for NewSpace {
    fn into_events(self) -> EntityEvents<SpaceEvent> {
        EntityEvents::init(
            self.id,
            [SpaceEvent::Initialized {
                id: self.id,
                slug: self.slug,
                description: self.description,
                authorized_projects: self.authorized_projects,
            }],
        )
    }
}

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};

    use super::{NewSpace, Space};
    use crate::primitives::{ProjectId, SpaceId};

    fn new_space(projects: Vec<ProjectId>) -> Space {
        let new = NewSpace::builder()
            .id(SpaceId::new())
            .slug("oncall")
            .description("On-call rotation".to_string())
            .authorized_projects(projects)
            .build()
            .unwrap();
        Space::try_from_events(new.into_events()).unwrap()
    }

    #[test]
    fn space_hydration_with_initial_project() {
        let project = ProjectId::new();
        let s = new_space(vec![project]);
        assert_eq!(s.slug, "oncall");
        assert_eq!(s.description.as_deref(), Some("On-call rotation"));
        assert_eq!(s.authorized_projects, vec![project]);
        assert!(s.is_project_authorized(project));
    }

    #[test]
    fn space_hydration_without_projects() {
        let new = NewSpace::builder()
            .id(SpaceId::new())
            .slug("incidents")
            .build()
            .unwrap();
        let s = Space::try_from_events(new.into_events()).unwrap();
        assert_eq!(s.slug, "incidents");
        assert_eq!(s.authorized_projects, Vec::<ProjectId>::new());
    }

    #[test]
    fn authorize_project_appends_and_is_idempotent() {
        let creator = ProjectId::new();
        let mut s = new_space(vec![creator]);

        let other = ProjectId::new();
        let res = s.authorize_project(other);
        assert!(res.did_execute());
        assert_eq!(s.authorized_projects, vec![creator, other]);

        let again = s.authorize_project(other);
        assert!(!again.did_execute());
        assert_eq!(s.authorized_projects, vec![creator, other]);
    }

    #[test]
    fn authorize_project_idempotent_for_creator() {
        let creator = ProjectId::new();
        let mut s = new_space(vec![creator]);
        let res = s.authorize_project(creator);
        assert!(!res.did_execute());
    }

    #[test]
    fn build_rejects_invalid_slug() {
        let res = NewSpace::builder().slug("Invalid Slug").build();
        assert!(matches!(
            res,
            Err(super::SpaceError::InvalidSlug { slug }) if slug == "Invalid Slug"
        ));
    }

    #[test]
    fn build_accepts_valid_slug() {
        let res = NewSpace::builder().slug("on-call").build();
        assert!(res.is_ok());
    }

    #[test]
    fn slug_accepts_simple_kebab() {
        assert!(super::validate_slug("oncall").is_ok());
        assert!(super::validate_slug("on-call").is_ok());
        assert!(super::validate_slug("incident-2024-q1").is_ok());
    }

    #[test]
    fn slug_rejects_empty() {
        assert!(super::validate_slug("").is_err());
    }

    #[test]
    fn slug_rejects_uppercase() {
        assert!(super::validate_slug("OnCall").is_err());
    }

    #[test]
    fn slug_rejects_path_separators() {
        assert!(super::validate_slug("on/call").is_err());
        assert!(super::validate_slug("on\\call").is_err());
        assert!(super::validate_slug("..").is_err());
    }

    #[test]
    fn slug_rejects_leading_or_trailing_hyphen() {
        assert!(super::validate_slug("-oncall").is_err());
        assert!(super::validate_slug("oncall-").is_err());
    }

    #[test]
    fn slug_rejects_double_hyphen() {
        assert!(super::validate_slug("on--call").is_err());
    }

    #[test]
    fn slug_rejects_special_chars() {
        assert!(super::validate_slug("on call").is_err());
        assert!(super::validate_slug("on.call").is_err());
        assert!(super::validate_slug("oncall!").is_err());
    }

    #[test]
    fn hydration_replays_authorize_events() {
        let id = SpaceId::new();
        let creator = ProjectId::new();
        let other = ProjectId::new();

        let events = es_entity::EntityEvents::init(
            id,
            [
                super::SpaceEvent::Initialized {
                    id,
                    slug: "oncall".to_string(),
                    description: None,
                    authorized_projects: vec![creator],
                },
                super::SpaceEvent::ProjectAuthorized { project_id: other },
            ],
        );
        let s = Space::try_from_events(events).unwrap();
        assert_eq!(s.authorized_projects, vec![creator, other]);
    }
}
