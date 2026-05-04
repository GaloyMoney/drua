mod error;

pub use error::LibraryError;

pub use drua_library::{
    DirEntry, DocType, GitFileHash, NewSpace, SearchHit, SearchableFields, Space, SpaceError,
    SpaceEvent, Spaces, WriteOp, SPACE_DOC_TYPE,
};

use std::sync::Arc;

use crate::audit::Audit;
use crate::auth::{AuthResource, AuthSubject, AuthVerb};
use crate::primitives::SpaceId;
use crate::user::Users;

const DEFAULT_SKILL_SYNC_INTERVAL_SECS: u64 = 20;

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct LibraryConfig {
    /// Defaults to `<repo-root>/.library/`.
    #[serde(default)]
    pub data_dir: Option<String>,
    #[serde(default)]
    pub repo_url: Option<String>,
    /// Default 20s. Reused as the upstream-fetch interval.
    #[serde(default = "default_skill_sync_interval_secs")]
    pub skill_sync_interval_secs: u64,
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            data_dir: None,
            repo_url: None,
            skill_sync_interval_secs: DEFAULT_SKILL_SYNC_INTERVAL_SECS,
        }
    }
}

fn default_skill_sync_interval_secs() -> u64 {
    DEFAULT_SKILL_SYNC_INTERVAL_SECS
}

impl LibraryConfig {
    pub fn repo_path(&self) -> std::path::PathBuf {
        match &self.data_dir {
            Some(d) => std::path::PathBuf::from(d).join("repo"),
            None => std::path::PathBuf::from(".library"),
        }
    }

    pub fn into_drua(self) -> drua_library::LibraryConfig {
        let data_dir = self.repo_path().to_string_lossy().into_owned();
        drua_library::LibraryConfig {
            data_dir,
            repo_url: self.repo_url.unwrap_or_default(),
            fetch_interval_ms: self.skill_sync_interval_secs.saturating_mul(1000).max(1000),
        }
    }
}

/// Auth-gated wrapper around `drua_library::Spaces`. Hosts the
/// `Space`-resource auth gate and per-method audit recording (action +
/// space_id) so callers downstream get accurate audit rows.
#[derive(Clone)]
pub struct AuthedSpaces {
    inner: drua_library::Spaces,
    users: Arc<Users>,
}

impl AuthedSpaces {
    pub fn new(inner: drua_library::Spaces, users: Arc<Users>) -> Self {
        Self { inner, users }
    }

    /// Persists a new `Space`, scaffolds `spaces/<slug>/.gitkeep`
    /// upstream, and waits for the upstream commit to land.
    #[tracing::instrument(name = "library.spaces.create", skip(self, sub))]
    pub async fn create(
        &self,
        sub: &AuthSubject,
        slug: impl Into<String> + std::fmt::Debug,
        description: Option<String>,
    ) -> Result<Space, LibraryError> {
        sub.can(AuthVerb::Create, AuthResource::Space(None))?;
        Audit::record_action_if_unset("library.spaces.create");
        let attribution = self.users.commit_attribution().await;
        let space = self
            .inner
            .create(slug.into(), description, attribution)
            .await?;
        Audit::record_space_id(space.id);
        tracing::info!(space.id = %space.id, space.slug = %space.slug, "space created");
        Ok(space)
    }

    /// Composable variant of [`Self::create`] — takes the caller's
    /// transaction so the space-create can be bundled with other
    /// writes atomically. The upstream `.gitkeep` push is *not*
    /// transactional with the DB op; on a successful op-commit the
    /// space exists locally and remotely, but a rolled-back op will
    /// not undo the upstream commit.
    #[tracing::instrument(name = "library.spaces.create_in_op", skip(self, op, sub))]
    pub async fn create_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        sub: &AuthSubject,
        slug: impl Into<String> + std::fmt::Debug,
        description: Option<String>,
    ) -> Result<Space, LibraryError> {
        sub.can(AuthVerb::Create, AuthResource::Space(None))?;
        Audit::record_action_if_unset("library.spaces.create");
        let attribution = self.users.commit_attribution().await;
        let space = self
            .inner
            .create_in_op(op, slug.into(), description, attribution)
            .await?;
        Audit::record_space_id(space.id);
        tracing::info!(space.id = %space.id, space.slug = %space.slug, "space created in op");
        Ok(space)
    }

    /// Lists every space, paginated. Soft-deleted spaces drop out at
    /// the SQL layer.
    #[tracing::instrument(name = "library.spaces.list_all", skip(self, sub))]
    pub async fn list_all(&self, sub: &AuthSubject) -> Result<Vec<Space>, LibraryError> {
        sub.can(AuthVerb::Read, AuthResource::Space(None))?;
        Audit::record_action_if_unset("library.spaces.list_all");
        Ok(self.inner.list_all().await?)
    }

    #[tracing::instrument(name = "library.spaces.find_by_slug", skip(self))]
    pub async fn find_by_slug(&self, slug: &str) -> Result<Option<Space>, LibraryError> {
        let space = self.inner.maybe_find_by_slug(slug).await?;
        if let Some(s) = &space {
            Audit::record_space_id(s.id);
        }
        Ok(space)
    }

    #[tracing::instrument(name = "library.spaces.find_by_ids", skip(self, ids))]
    pub async fn find_by_ids(&self, ids: &[SpaceId]) -> Result<Vec<Space>, LibraryError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self.inner.find_by_ids(ids).await?)
    }

    /// Direct accessor for sandboxless file ops (`SpaceFs`). Read/write
    /// methods on `Spaces` are themselves un-authed; callers are
    /// expected to gate per-space access (e.g. via
    /// `Projects::space_for_subject`) before invoking them.
    pub fn inner(&self) -> &drua_library::Spaces {
        &self.inner
    }
}

/// Auth-gated wrapper around `drua_library::SearchStore`. Library
/// content is globally readable to any authenticated subject; the
/// gate here is just non-anonymous.
#[derive(Clone)]
pub struct AuthedSearch {
    inner: drua_library::SearchStore,
}

impl AuthedSearch {
    pub fn new(inner: drua_library::SearchStore) -> Self {
        Self { inner }
    }

    /// Cross-scope hybrid FTS + semantic search. Empty `scope_ids` =
    /// no scope filter; otherwise hits are restricted to those
    /// scope_ids plus globals (null scope_id). Empty `path_prefixes`
    /// = no path filter; non-empty restricts hits to documents whose
    /// `path` is exactly one of the prefixes or lives under one as a
    /// subtree. Tree-prefix semantics — see
    /// `drua_library::SearchStore::search`.
    #[tracing::instrument(
        name = "library.search.search",
        skip(self, sub, scope_ids, path_prefixes)
    )]
    pub async fn search(
        &self,
        sub: &AuthSubject,
        scope_ids: &[uuid::Uuid],
        query: &str,
        doc_types: &[DocType],
        path_prefixes: &[String],
        limit: usize,
    ) -> Result<Vec<SearchHit>, LibraryError> {
        if matches!(sub, AuthSubject::Anonymous) {
            return Err(crate::auth::error::AuthorizationError::AuthenticationRequired.into());
        }
        Audit::record_action_if_unset("library.search.search");
        Ok(self
            .inner
            .search(query, None, scope_ids, doc_types, path_prefixes, limit)
            .await?)
    }

    #[tracing::instrument(name = "library.search.find_by_ids", skip(self, sub, ids))]
    pub async fn find_by_ids(
        &self,
        sub: &AuthSubject,
        ids: &[uuid::Uuid],
    ) -> Result<Vec<SearchableFields>, LibraryError> {
        if matches!(sub, AuthSubject::Anonymous) {
            return Err(crate::auth::error::AuthorizationError::AuthenticationRequired.into());
        }
        Audit::record_action_if_unset("library.search.find_by_ids");
        Ok(self.inner.find_by_ids(ids).await?)
    }
}
