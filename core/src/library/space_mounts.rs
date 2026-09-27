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
use crate::workflow::SpaceWritesMode;

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

/// rev6 D49: interactive agents are read-only on spaces — the one
/// sentence `render_spaces_block`'s `write_line` and `whoami`'s
/// `space_write_mode_note` both render verbatim for every non-admin
/// subject. Spaces are edited by a workflow (`space_writes:`) or by an
/// admin (`drua_admin_spaces` with `target: draft`), never from here.
pub const SPACE_WRITE_MODE_SENTENCE: &str =
    "Spaces are read-only in this session — edits to a space are made \
     by a workflow (`space_writes:`) or by an admin.\n";

/// rev5 D43: which write line `render_spaces_block` renders. `None`
/// for `WorkflowRun` — the run has no draft, so `space:` is refused.
#[derive(Debug, Clone, Copy)]
pub enum SpacesBlockMode {
    /// Leads, members, `ExportedAgent`, admins — rev3's one sentence.
    Interactive,
    /// A workflow-run step agent. `space:<slug>/` overlays the run's
    /// draft (rev4 D25) whenever `mode != read_only`; there is no
    /// `draft:` prefix or draft command to reach for.
    WorkflowRun(SpaceWritesMode),
}

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
    /// in `project_id`. `Ok(None)` when no spaces are mounted. rev3
    /// §6.3 / rev5 D43: one sentence per `mode` — `Interactive`'s
    /// write mode never varies by authority (`space:` fails closed per
    /// D15); a `WorkflowRun` mode instead says what happens to the
    /// run's overlay draft.
    #[instrument(name = "library.space_mounts.spaces_block_for_project", skip(self))]
    pub async fn spaces_block_for_project(
        &self,
        project_id: ProjectId,
        mode: SpacesBlockMode,
    ) -> Result<Option<String>, SpaceMountsError> {
        let spaces = self.spaces_for_project(project_id).await?;
        Ok(render_spaces_block(&spaces, mode))
    }
}

fn render_spaces_block(spaces: &[Space], mode: SpacesBlockMode) -> Option<String> {
    if spaces.is_empty() {
        return None;
    }
    let total = spaces.len();

    let write_line = match mode {
        SpacesBlockMode::Interactive => SPACE_WRITE_MODE_SENTENCE.to_string(),
        SpacesBlockMode::WorkflowRun(write_mode) => workflow_run_write_line(write_mode),
    };
    // rev6 D49: an interactive agent's file tools can only read a space
    // (D44) — no `Edit`/`Move`/`Delete`, no `draft:` prefix. A lead
    // (no file tools at all — `can_use_agent_file_tools` is agents-only,
    // unchanged by this rev) reaches even reads only via `spaces view`.
    // A run's tools still write (rev5 D25 unchanged), so its header
    // keeps the full list and the `draft:` clause.
    let tools_line = match mode {
        SpacesBlockMode::Interactive => {
            "Use the file tools (Read, LS, Glob, Grep) — or `spaces view` \
             if you are the project lead — with paths prefixed \
             `space:<slug>/` to read their contents."
        }
        SpacesBlockMode::WorkflowRun(_) => {
            "Use the file tools (Read, LS, Glob, Grep, Edit, Move, \
             Delete) with paths prefixed `space:<slug>/` (or \
             `draft:<slug>/` to always stage) to read or write their \
             contents."
        }
    };
    let header = format!(
        "<spaces>\n\
         This project has the following knowledge spaces mounted — \
         collaborative folders backed by a shared library. {tools_line} {write_line}"
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

/// rev5 §6.3/D43: one sentence per `space_writes.mode`, so a step
/// agent knows whether its edits will merge, become a PR, or be
/// refused outright — the run overlays its draft (rev4 D25), so there
/// is no `draft:` prefix or draft command to reach for either way.
fn workflow_run_write_line(mode: SpaceWritesMode) -> String {
    match mode {
        SpaceWritesMode::Merge => "This run's edits to space:<slug>/ paths are staged in a \
             draft the workflow owns and are merged to the published library when the run \
             succeeds. Use the file tools with space:<slug>/ paths as usual — no draft: prefix \
             and no draft commands. library_search sees the published library only.\n"
            .to_string(),
        SpaceWritesMode::OpenPr => "This run's edits to space:<slug>/ paths are staged in a \
             draft the workflow owns and are opened as a pull request for review when the run \
             succeeds. Use the file tools with space:<slug>/ paths as usual — no draft: prefix \
             and no draft commands. library_search sees the published library only.\n"
            .to_string(),
        SpaceWritesMode::ReadOnly => {
            "Spaces are read-only in this run; space:<slug>/ paths can be read but not written.\n"
                .to_string()
        }
    }
}
