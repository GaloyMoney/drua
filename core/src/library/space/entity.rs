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
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Space {
    pub id: SpaceId,
    pub slug: String,
    #[builder(setter(strip_option), default)]
    pub description: Option<String>,
    events: EntityEvents<SpaceEvent>,
}

impl Space {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
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

        for event in events.iter_all() {
            match event {
                SpaceEvent::Initialized {
                    id,
                    slug,
                    description,
                } => {
                    builder = builder.id(*id).slug(slug.clone());
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
            }],
        )
    }
}

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};

    use super::{NewSpace, Space};

    fn new_space() -> Space {
        let new = NewSpace::builder()
            .id(crate::primitives::SpaceId::new())
            .slug("oncall")
            .description("On-call rotation".to_string())
            .build()
            .unwrap();
        Space::try_from_events(new.into_events()).unwrap()
    }

    #[test]
    fn space_hydration() {
        let s = new_space();
        assert_eq!(s.slug, "oncall");
        assert_eq!(s.description.as_deref(), Some("On-call rotation"));
    }

    #[test]
    fn space_hydration_without_description() {
        let new = NewSpace::builder()
            .id(crate::primitives::SpaceId::new())
            .slug("incidents")
            .build()
            .unwrap();
        let s = Space::try_from_events(new.into_events()).unwrap();
        assert_eq!(s.slug, "incidents");
        assert_eq!(s.description, None);
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
}
