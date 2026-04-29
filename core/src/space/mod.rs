mod entity;
pub mod error;
pub(crate) mod repo;

use tracing::instrument;

pub use entity::{NewSpace, Space, SpaceEvent};
pub use error::*;
use repo::*;

use crate::audit::Audit;
use crate::library::Library;
use crate::primitives::*;

#[derive(Clone)]
pub struct Spaces {
    repo: SpaceRepo,
    library: Library,
}

impl Spaces {
    pub fn new(pool: &sqlx::PgPool, library: Library) -> Self {
        Self {
            repo: SpaceRepo::new(pool),
            library,
        }
    }

    /// Persists the entity and queues a `SpaceInit` op so the library
    /// gets `spaces/<slug>/.gitkeep` committed in the same transaction.
    /// The creating subject's workspace is seeded into
    /// `authorized_workspaces`; non-agent subjects (e.g. plain `User`)
    /// produce a space with an empty authorized list.
    #[instrument(name = "domain.space.create", skip(self, sub))]
    pub async fn create(
        &self,
        sub: &AuthSubject,
        slug: impl Into<String> + std::fmt::Debug,
        description: Option<String>,
    ) -> Result<Space, SpaceError> {
        sub.can(AuthVerb::Create, AuthResource::Space(None))?;
        Audit::record_action_if_unset("space.create");

        let slug = slug.into();
        validate_slug(&slug)?;

        let initial_workspaces: Vec<WorkspaceId> = sub.workspace_id().into_iter().collect();

        let mut builder = NewSpace::builder();
        builder.slug(slug);
        if let Some(desc) = description {
            builder.description(desc);
        }
        builder.authorized_workspaces(initial_workspaces);
        let new_space = builder.build().expect("could not build new space");

        let mut op = self.repo.begin_op().await?;
        let space = self.repo.create_in_op(&mut op, new_space).await?;
        self.library
            .sync_space_folder_in_op(&mut op, &space.slug)
            .await?;
        op.commit().await?;

        tracing::info!(space.id = %space.id, space.slug = %space.slug, "space created");
        Ok(space)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_accepts_simple_kebab() {
        assert!(validate_slug("oncall").is_ok());
        assert!(validate_slug("on-call").is_ok());
        assert!(validate_slug("incident-2024-q1").is_ok());
    }

    #[test]
    fn slug_rejects_empty() {
        assert!(validate_slug("").is_err());
    }

    #[test]
    fn slug_rejects_uppercase() {
        assert!(validate_slug("OnCall").is_err());
    }

    #[test]
    fn slug_rejects_path_separators() {
        assert!(validate_slug("on/call").is_err());
        assert!(validate_slug("on\\call").is_err());
        assert!(validate_slug("..").is_err());
    }

    #[test]
    fn slug_rejects_leading_or_trailing_hyphen() {
        assert!(validate_slug("-oncall").is_err());
        assert!(validate_slug("oncall-").is_err());
    }

    #[test]
    fn slug_rejects_double_hyphen() {
        assert!(validate_slug("on--call").is_err());
    }

    #[test]
    fn slug_rejects_special_chars() {
        assert!(validate_slug("on call").is_err());
        assert!(validate_slug("on.call").is_err());
        assert!(validate_slug("oncall!").is_err());
    }
}
