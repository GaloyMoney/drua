//! Read-side facade for the project↔space mount relationship.
//!
//! Today the relationship is stored as a list-field on the `Project`
//! entity (`Project.mounted_spaces: Vec<SpaceId>`). The facade hides
//! that detail so callers never reach into `Projects` (or `ProjectRepo`)
//! directly — when the storage shape grows up to a dedicated
//! `project_space_mounts` join table, no caller changes.
//!
//! Read-only by design. Mounts are written exclusively through
//! `Projects::{mount,unmount}_space*` (auth-checked, event-sourced).
//! `SpaceMounts` is unauthenticated — it's an internal data accessor
//! used by `Agents` and similar internal renderers; public surfaces go
//! through `Projects`.

use std::sync::Arc;

use drua_library::Space;
use thiserror::Error;
use tracing::instrument;

use crate::library::{AuthedSpaces, LibraryError};
use crate::primitives::{ProjectId, SpaceId};
use crate::project::repo::{ProjectFindError, ProjectRepo};

#[derive(Error, Debug)]
pub enum SpaceMountsError {
    #[error("project lookup: {0}")]
    Project(#[from] ProjectFindError),
    #[error("library: {0}")]
    Library(#[from] LibraryError),
}

/// Cap on the rendered `<spaces>` system block — above this we render a
/// truncation footer; agents enumerate the rest via the `spaces` tool.
const SPACES_BLOCK_LIMIT: usize = 20;

#[derive(Clone)]
pub struct SpaceMounts {
    /// `None` in test contexts (`empty()`) where no library is wired up.
    /// All lookups short-circuit to empty in that case — the consumer
    /// fallback in `Agents::cached_dynamic_blocks` renders skills
    /// without the space tier.
    inner: Option<Inner>,
}

#[derive(Clone)]
struct Inner {
    project_repo: Arc<ProjectRepo>,
    spaces: Arc<AuthedSpaces>,
}

impl SpaceMounts {
    pub fn new(project_repo: Arc<ProjectRepo>, spaces: Arc<AuthedSpaces>) -> Self {
        Self {
            inner: Some(Inner {
                project_repo,
                spaces,
            }),
        }
    }

    /// Test-only constructor that returns empty results from every
    /// lookup. Use in tests that exercise `Agents` without standing up
    /// the full library + projects stack.
    pub fn empty() -> Self {
        Self { inner: None }
    }

    /// Mount IDs for `project_id`. Used by `AgentScope` to derive which
    /// space-scoped skills the agent can see.
    #[instrument(name = "library.space_mounts.space_ids_for_project", skip(self))]
    pub async fn space_ids_for_project(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<SpaceId>, SpaceMountsError> {
        let Some(inner) = self.inner.as_ref() else {
            return Ok(Vec::new());
        };
        let project = inner.project_repo.find_by_id(project_id).await?;
        Ok(project.mounted_spaces.clone())
    }

    /// Full `Space` entities (slug + description) for `project_id`. Used
    /// by the `<spaces>` system-block renderer and any other read-only
    /// caller that needs the entities — not just the IDs.
    #[instrument(name = "library.space_mounts.spaces_for_project", skip(self))]
    pub async fn spaces_for_project(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<Space>, SpaceMountsError> {
        let Some(inner) = self.inner.as_ref() else {
            return Ok(Vec::new());
        };
        let project = inner.project_repo.find_by_id(project_id).await?;
        if project.mounted_spaces.is_empty() {
            return Ok(Vec::new());
        }
        Ok(inner.spaces.find_by_ids(&project.mounted_spaces).await?)
    }

    /// Rendered `<spaces>...</spaces>` system-prompt block for an agent
    /// in `project_id`. `Ok(None)` when no spaces are mounted.
    /// `can_write_main` — subject-aware per rev2 §6.3 (amending the
    /// handoff's §8.3): `true` (a lead's `ProjectAdmin` scope) keeps
    /// direct `space:` writes and offers `draft:` to stage instead;
    /// `false` (`Propose`-only — ordinary task/step agents) says
    /// `space:` itself stages, so it doesn't promise a write mode the
    /// subject doesn't have. Neither line mentions "changeset" or an
    /// "open"/"bind" step — rev2 D2/D4 removed both.
    #[instrument(name = "library.space_mounts.spaces_block_for_project", skip(self))]
    pub async fn spaces_block_for_project(
        &self,
        project_id: ProjectId,
        can_write_main: bool,
    ) -> Result<Option<String>, SpaceMountsError> {
        let spaces = self.spaces_for_project(project_id).await?;
        Ok(render_spaces_block(&spaces, can_write_main))
    }
}

fn render_spaces_block(spaces: &[Space], can_write_main: bool) -> Option<String> {
    if spaces.is_empty() {
        return None;
    }
    let total = spaces.len();

    let write_line = if can_write_main {
        "Edits to space:<slug>/ paths save straight to the library. Use \
         draft:<slug>/ paths instead to stage them; `spaces publish` \
         then lands the draft on main.\n"
    } else {
        "Edits to space:<slug>/ paths save to your unpublished draft, \
         never straight to the library. `spaces publish` sends the \
         draft for review as a GitHub PR.\n"
    };
    let header = format!(
        "<spaces>\n\
         This project has the following knowledge spaces mounted — \
         collaborative folders backed by a shared library. Use the \
         file tools (Read, LS, Glob, Grep, Edit, Move, Delete) with \
         paths prefixed `space:<slug>/` (or `draft:<slug>/` to always \
         stage) to read or write their contents. {write_line}"
    );

    let mut buf = header;
    for s in spaces.iter().take(SPACES_BLOCK_LIMIT) {
        match s.description.as_deref() {
            Some(d) if !d.is_empty() => {
                buf.push_str(&format!("- space:{} — {}\n", s.slug, d));
            }
            _ => buf.push_str(&format!("- space:{}\n", s.slug)),
        }
    }
    if total > SPACES_BLOCK_LIMIT {
        buf.push_str(&format!(
            "…and {} more (use the `spaces` tool with command `list` to enumerate).\n",
            total - SPACES_BLOCK_LIMIT,
        ));
    }
    buf.push_str("</spaces>\n");
    Some(buf)
}
