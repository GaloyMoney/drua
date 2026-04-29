mod entity;
pub mod error;
pub mod file_sync;
pub(crate) mod repo;

use tracing::instrument;

pub use entity::{NewSpace, Space, SpaceEvent};
pub use error::*;
use repo::*;

use super::Library;
use crate::audit::Audit;
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
    /// gets `spaces/<slug>/.gitkeep` committed in the same transaction
    /// (mirrors `Workspaces::create`'s explicit `sync_workspace_folder_in_op`
    /// call). The creating subject's workspace is seeded into
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

        let initial_workspaces: Vec<WorkspaceId> = sub.workspace_id().into_iter().collect();

        let mut builder = NewSpace::builder();
        builder.slug(slug.into());
        if let Some(desc) = description {
            builder.description(desc);
        }
        builder.authorized_workspaces(initial_workspaces);
        let new_space = builder.build()?;

        let mut op = self.repo.begin_op().await?;
        let space = self.repo.create_in_op(&mut op, new_space).await?;
        self.library
            .sync_space_folder_in_op(&mut op, &space.slug)
            .await?;
        op.commit().await?;

        tracing::info!(space.id = %space.id, space.slug = %space.slug, "space created");
        Ok(space)
    }

    /// No-auth slug → space lookup. Used by the library file-sync job
    /// when resolving `spaces/<slug>/...` paths to space ids.
    pub(crate) async fn find_by_slug(&self, slug: &str) -> Result<Option<Space>, SpaceError> {
        Ok(self.repo.maybe_find_by_slug(slug).await?)
    }

    /// Resolves a slug to a `Space` after enforcing two checks:
    /// 1. The subject can read `AuthResource::Space(Some(space.id))`
    ///    (workspace admins are blanket-allowed by the scope layer).
    /// 2. The subject's workspace is in `space.authorized_workspaces`
    ///    — the per-space membership list.
    ///
    /// Used by sandbox-creation flows that need to honor space ACLs
    /// without exposing the lower-level `find_by_slug` to callers.
    #[instrument(name = "domain.space.find_by_slug_authorized", skip(self, sub))]
    pub async fn find_by_slug_authorized(
        &self,
        sub: &AuthSubject,
        slug: &str,
    ) -> Result<Space, SpaceError> {
        let space = self
            .repo
            .maybe_find_by_slug(slug)
            .await?
            .ok_or_else(|| SpaceError::NotFound {
                slug: slug.to_string(),
            })?;

        sub.can(AuthVerb::Read, AuthResource::Space(Some(space.id)))?;
        Audit::record_action_if_unset("space.find_by_slug");

        let workspace_id = sub
            .workspace_id()
            .ok_or(AuthorizationError::AuthenticationRequired)?;
        if !space.is_workspace_authorized(workspace_id) {
            return Err(SpaceError::WorkspaceNotAuthorized {
                slug: space.slug.clone(),
                workspace_id,
            });
        }

        Ok(space)
    }
}
