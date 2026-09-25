use std::sync::{Arc, LazyLock};

use rmcp::model::{CallToolResult, JsonObject};
use serde::Deserialize;

use drua_library::{Space, SPACE_DOC_TYPE};

use crate::audit::Audit;
use crate::auth::{AuthResource, AuthScope, AuthSubject, AuthVerb};
use crate::changeset::{Changeset, ChangesetStatus, Changesets, TouchedFile, TouchedKind};
use crate::library::{AuthedSearch, AuthedSpaces};
use crate::primitives::ChangesetId;
use crate::project::Projects;
use crate::space_fs::SpaceFs;

use super::super::error::ToolSetsError;
use super::super::inspect::{dispatch_edit, dispatch_view, EditOp, ReadOp};
use super::super::traits::TopLevelTool;
use super::{parse_params, OutputSchema};

fn default_search_limit() -> usize {
    10
}

#[derive(Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
enum SpacesParams {
    Create {
        slug: String,
        #[serde(default)]
        description: Option<String>,
    },
    /// Mounts an existing space onto the calling agent's project so
    /// it shows up in `<spaces>` and is accessible via `space:<slug>/`
    /// paths.
    Mount { slug: String },
    /// Drops a space from the calling agent's project. Idempotent;
    /// the space itself is unaffected.
    Unmount { slug: String },
    /// Lists spaces. Defaults to spaces mounted on the caller's
    /// project; `all: true` returns every space in the library
    /// (used to discover candidates before `mount`).
    List {
        #[serde(default)]
        all: bool,
    },
    /// Read-only file ops on a space mounted on the caller's project.
    /// `op` selects the sub-tool; `op_args` shape depends on it.
    View {
        slug: String,
        op: ReadOp,
        #[serde(default)]
        op_args: Option<JsonObject>,
    },
    /// Mutating file ops on a space mounted on the caller's project.
    /// `op` selects the sub-tool; `op_args` shape depends on it.
    Edit {
        slug: String,
        op: EditOp,
        #[serde(default)]
        op_args: Option<JsonObject>,
    },
    /// Hybrid FTS + semantic search restricted to a single mounted
    /// space. Mirrors `notes.search` / skill `use_skill search`, but
    /// scoped to `space_file` rows tagged with this space's id.
    /// `paths` (optional) narrows the candidate pool to files whose
    /// path is exactly one of the entries or lives under one as a
    /// subtree (`triggers/` matches `triggers/x.md` but not
    /// `triggersfoo.md`). Empty / omitted = whole space.
    Search {
        slug: String,
        query: String,
        #[serde(default)]
        paths: Vec<String>,
        #[serde(default = "default_search_limit")]
        limit: usize,
    },
    /// The caller's own open draft (rev2 §6.2), or `{draft: null}` if
    /// none is open yet. No `id` — this is always "mine".
    Draft,
    /// Changesets in the caller's project, newest first (rev1's
    /// `changeset list`, folded in here per rev2 §6).
    Drafts {
        #[serde(default)]
        status: Option<ChangesetStatus>,
    },
    /// Sends a draft for review or landing — effect resolved by
    /// authority (rev2 D6): a subject holding `Update` on spaces lands
    /// it on `main`; a `Propose`-only subject opens a GitHub PR.
    /// `id` defaults to the caller's own open draft; leads/admins may
    /// pass another context's draft id.
    Publish {
        #[serde(default)]
        id: Option<ChangesetId>,
    },
    /// Closes a draft without landing it. `id` defaults to the
    /// caller's own open draft.
    Discard {
        #[serde(default)]
        id: Option<ChangesetId>,
        #[serde(default)]
        reason: Option<String>,
    },
    /// Moves a draft onto current `main`, squashing its commits. `id`
    /// defaults to the caller's own open draft. Conflicts are reported
    /// without touching the branch.
    Rebase {
        #[serde(default)]
        id: Option<ChangesetId>,
    },
}

impl SpacesParams {
    fn command_name(&self) -> &'static str {
        match self {
            Self::Create { .. } => "create",
            Self::Mount { .. } => "mount",
            Self::Unmount { .. } => "unmount",
            Self::List { .. } => "list",
            Self::View { .. } => "view",
            Self::Edit { .. } => "edit",
            Self::Search { .. } => "search",
            Self::Draft => "draft",
            Self::Drafts { .. } => "drafts",
            Self::Publish { .. } => "publish",
            Self::Discard { .. } => "discard",
            Self::Rebase { .. } => "rebase",
        }
    }
}

#[derive(Default, serde::Serialize, schemars::JsonSchema)]
struct SpaceSummary {
    id: String,
    slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

impl From<&Space> for SpaceSummary {
    fn from(s: &Space) -> Self {
        Self {
            id: s.id.to_string(),
            slug: s.slug.clone(),
            description: s.description.clone(),
        }
    }
}

#[derive(Default, serde::Serialize, schemars::JsonSchema)]
struct SpacesOutput {
    command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    space: Option<SpaceSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    spaces: Option<Vec<SpaceSummary>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    results: Option<Vec<SpaceSearchHit>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    draft: Option<ChangesetSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    drafts: Option<Vec<ChangesetSummary>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commits: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mergeable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    conflicts: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    touched: Option<Vec<TouchedFileOut>>,
    /// `publish` outcome — `"pr_opened"` or `"landed"` (rev2 D6).
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pr_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pr_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    merge_oid: Option<String>,
}

#[derive(Default, serde::Serialize, schemars::JsonSchema)]
struct ChangesetSummary {
    id: String,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    status: String,
    branch: String,
    base_oid: String,
    head_oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pr_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pr_url: Option<String>,
}

impl From<&Changeset> for ChangesetSummary {
    fn from(cs: &Changeset) -> Self {
        Self {
            id: cs.id.to_string(),
            title: cs.title.clone(),
            description: cs.description.clone(),
            status: format!("{:?}", cs.status).to_lowercase(),
            branch: cs.branch(),
            base_oid: cs.base_oid.clone(),
            head_oid: cs.head_oid.clone(),
            pr_number: cs.pr_number,
            pr_url: cs.pr_url.clone(),
        }
    }
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct TouchedFileOut {
    space: String,
    path: String,
    kind: String,
}

impl From<&TouchedFile> for TouchedFileOut {
    fn from(t: &TouchedFile) -> Self {
        Self {
            space: t.space_slug.clone(),
            path: t.path.clone(),
            kind: match t.kind {
                TouchedKind::Added => "added",
                TouchedKind::Modified => "modified",
                TouchedKind::Deleted => "deleted",
            }
            .to_string(),
        }
    }
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct SpaceSearchHit {
    doc_id: String,
    title: String,
    preview: String,
    score: f64,
    /// Path inside `spaces/<slug>/`, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    relative_path: Option<String>,
}

static SPACES_OUTPUT: LazyLock<OutputSchema<SpacesOutput>> = LazyLock::new(OutputSchema::new);

static SPACES_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "enum": ["create", "mount", "unmount", "list", "view", "edit", "search", "draft", "drafts", "publish", "discard", "rebase"],
                "description": "Which spaces operation to perform."
            },
            "slug": {
                "type": "string",
                "description": "Directory-safe identifier ([a-z0-9-]+, no leading/trailing hyphens). Becomes spaces/<slug>/ in the library repo. Required for create, mount, unmount, view, and edit."
            },
            "description": {
                "type": "string",
                "description": "Human-readable summary of the space's purpose. Used by create only."
            },
            "all": {
                "type": "boolean",
                "description": "List flag: true returns every space in the library (for discovery before mount); false (default) returns only spaces mounted on this project."
            },
            "op": {
                "type": "string",
                "enum": ["read", "ls", "grep", "glob", "write", "str_replace", "insert", "delete", "move"],
                "description": "Sub-op for view (read|ls|grep|glob) or edit (write|str_replace|insert|delete|move)."
            },
            "op_args": {
                "type": "object",
                "description": "Sub-op arguments. view: read/ls take {path, ...} (ls also takes details? — append each file's first-/last-commit dates); grep/glob take {pattern, path?, ...} (glob also takes details?). edit: write {path, content}; str_replace {path, old_str, new_str}; insert {path, line, text}; delete {path}; move {from, to}."
            },
            "query": {
                "type": "string",
                "description": "Search query — keywords or natural language (search command)."
            },
            "paths": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Optional path-prefix scope for `search`: each entry restricts hits to files whose path equals it or lives under it as a subtree (`triggers/` matches `triggers/x.md` but not `triggersfoo.md`). Trailing slash is optional. Empty / omitted = whole space. Reject leading `/`, `..` segments, and glob metacharacters."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "description": "Maximum number of search results (search command, default 10)."
            },
            "id": {
                "type": "string",
                "description": "Changeset id (uuid). Optional for publish/discard/rebase — defaults to the caller's own open draft; leads/admins may target another context's draft."
            },
            "status": {
                "type": "string",
                "enum": ["open", "submitted", "merged", "applied", "discarded", "abandoned"],
                "description": "Optional status filter for drafts."
            },
            "reason": {
                "type": "string",
                "description": "Optional free-text reason. Used by discard."
            }
        },
        "required": ["command"],
        "additionalProperties": false
    })
});

pub struct SpacesTool {
    spaces: Arc<AuthedSpaces>,
    projects: Arc<Projects>,
    space_fs: Arc<SpaceFs>,
    search: Arc<AuthedSearch>,
    changesets: Arc<Changesets>,
}

impl SpacesTool {
    pub fn new(
        spaces: Arc<AuthedSpaces>,
        projects: Arc<Projects>,
        space_fs: Arc<SpaceFs>,
        search: Arc<AuthedSearch>,
        changesets: Arc<Changesets>,
    ) -> Self {
        Self {
            spaces,
            projects,
            space_fs,
            search,
            changesets,
        }
    }

    /// `id` if given; else the caller's own open draft. Shared by
    /// `publish`/`discard`/`rebase` (rev2 §6.2).
    async fn resolve_draft_id(
        &self,
        sub: &AuthSubject,
        id: Option<ChangesetId>,
    ) -> Result<ChangesetId, ToolSetsError> {
        if let Some(id) = id {
            return Ok(id);
        }
        self.changesets
            .open_draft_for(sub)
            .await?
            .map(|cs| cs.id)
            .ok_or_else(|| {
                ToolSetsError::InvalidArgument(
                    "no changeset id given and the caller has no open draft".to_string(),
                )
            })
    }

    /// Validate and normalise caller-supplied path prefixes for any
    /// space-scoped search (`spaces.search`, `drua_admin_spaces`
    /// `search`, `library_search.paths`). Empty entries are dropped;
    /// trailing slashes stripped. Leading `/`, `..` segments, and
    /// glob metacharacters (`*`, `?`, `[`) are rejected — globs are
    /// out of scope.
    pub(crate) fn normalize_path_prefixes(
        paths: Vec<String>,
    ) -> Result<Vec<String>, ToolSetsError> {
        let mut out = Vec::with_capacity(paths.len());
        for raw in paths {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with('/') {
                return Err(ToolSetsError::InvalidArgument(format!(
                    "path prefix entry must not start with '/': {raw:?}"
                )));
            }
            if trimmed.split('/').any(|seg| seg == "..") {
                return Err(ToolSetsError::InvalidArgument(format!(
                    "path prefix entry must not contain '..': {raw:?}"
                )));
            }
            if trimmed.contains(['*', '?', '[']) {
                return Err(ToolSetsError::InvalidArgument(format!(
                    "path prefix does not support glob patterns: {raw:?}"
                )));
            }
            let normalized = trimmed.trim_end_matches('/');
            if normalized.is_empty() {
                continue;
            }
            out.push(normalized.to_string());
        }
        Ok(out)
    }
}

#[async_trait::async_trait]
impl TopLevelTool for SpacesTool {
    fn name(&self) -> &str {
        "spaces"
    }

    fn description(&self) -> &str {
        "Manage library spaces — bounded collaborative folders under \
         `spaces/<slug>/` in the knowledge-base repo. Commands: \
         `create` (requires `slug`, optional `description`; auto-mounts \
         onto the caller's project; leads/admins only), \
         `mount` / `unmount` (requires `slug`; idempotent; leads/admins only), \
         `list` (defaults to spaces mounted by the caller's project; \
         pass `all: true` to discover every space in the library), \
         `view` (read-only file ops; requires `slug`, `op`, `op_args`; \
         op=read {path, offset?, limit?}, ls {path, details?}, \
         grep {pattern, path?, glob?, output_mode?, ...}, \
         glob {pattern, path?, details?}), \
         `edit` (mutating file ops; requires `slug`, `op`, `op_args`; \
         op=write {path, content} (full overwrite), \
         str_replace {path, old_str, new_str} (old_str must occur once), \
         insert {path, line, text} (line is 1-based, insert AFTER; 0 prepends), \
         delete {path}, move {from, to}), \
         `search` (hybrid FTS + semantic search over the files in a \
         single mounted space; requires `slug`, `query`; optional \
         `paths` is a list of subtree prefixes (e.g. \
         [\"triggers/\", \"runbooks/\"]) — empty = whole space; \
         optional `limit` defaults to 10), \
         `draft` (your own open draft, or {draft: null}), \
         `drafts` (drafts in your project, newest first; optional `status` filter), \
         `publish` (sends a draft for review or landing — effect decided \
         by your own write authority, not a choice you make; optional `id` \
         defaults to your own open draft), \
         `discard` (closes a draft without landing it; optional `id`/`reason`), \
         `rebase` (moves a draft onto current `main`, squashing; optional `id`). \
         File ops and search are gated on the slug being mounted on \
         the caller's project. Use path=\"\" for the space root."
    }

    fn input_schema(&self) -> &serde_json::Value {
        &SPACES_SCHEMA
    }

    fn inner_output_schema(&self) -> Option<&serde_json::Value> {
        Some(SPACES_OUTPUT.schema())
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        // Leads/admins see the whole tool (create/mount/unmount still
        // enforce `Update` on `Project` themselves, defense in depth).
        // rev2: also visible to any subject that can at least stage a
        // space edit (`Propose`) — the draft/drafts/publish/discard/
        // rebase commands are theirs to use; `call()` doesn't gate
        // those further since `Changesets`/`SpaceFs` already do.
        //
        // `WorkflowScript`-marked subjects are excluded even though
        // `ProjectMember` grants them `Propose` on `Space` — a script
        // step gets only the direct file-manipulation tools
        // (`can_use_agent_file_tools`), never a management tool; it has
        // no interactive turn to run `spaces publish` from, and the
        // executor already closes its run's draft on its behalf.
        if subject.scopes().contains(&AuthScope::WorkflowScript) {
            return false;
        }
        subject.effective_project_id().is_some_and(|p| {
            subject
                .can(AuthVerb::Update, AuthResource::Project(Some(p)))
                .is_ok()
                || subject
                    .can(AuthVerb::Propose, AuthResource::Space(None))
                    .is_ok()
                || subject
                    .can(AuthVerb::Update, AuthResource::Space(None))
                    .is_ok()
        })
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let project_id = subject
            .effective_project_id()
            .ok_or(ToolSetsError::Unauthorized)?;
        let params: SpacesParams = parse_params(arguments)?;
        Audit::record_action(format!("spaces.{}", params.command_name()));

        let (text, out) = match params {
            SpacesParams::View { slug, op, op_args } => {
                return dispatch_view(
                    &self.space_fs,
                    subject,
                    &slug,
                    op,
                    op_args.unwrap_or_default(),
                )
                .await;
            }
            SpacesParams::Edit { slug, op, op_args } => {
                return dispatch_edit(
                    &self.space_fs,
                    subject,
                    &slug,
                    op,
                    op_args.unwrap_or_default(),
                )
                .await;
            }
            SpacesParams::Create { slug, description } => {
                let space = self
                    .projects
                    .create_and_mount_space(subject, project_id, slug, description)
                    .await?;

                let text = format!("Space created.\n  id: {}\n  slug: {}", space.id, space.slug);
                let out = SpacesOutput {
                    command: "create".to_string(),
                    space: Some(SpaceSummary::from(&space)),
                    ..Default::default()
                };
                (text, out)
            }
            SpacesParams::Mount { slug } => {
                let space = self
                    .projects
                    .mount_space(subject, project_id, &slug)
                    .await?;

                let text = format!(
                    "Space mounted onto project {}.\n  slug: {}",
                    project_id, space.slug
                );
                let out = SpacesOutput {
                    command: "mount".to_string(),
                    space: Some(SpaceSummary::from(&space)),
                    ..Default::default()
                };
                (text, out)
            }
            SpacesParams::Unmount { slug } => {
                let space = self.spaces.find_by_slug(&slug).await?.ok_or_else(|| {
                    ToolSetsError::Library(
                        drua_library::SpaceError::NotFound { slug: slug.clone() }.into(),
                    )
                })?;
                self.projects
                    .unmount_space(subject, project_id, space.id)
                    .await?;

                let text = format!(
                    "Space unmounted from project {}.\n  slug: {}",
                    project_id, space.slug
                );
                let out = SpacesOutput {
                    command: "unmount".to_string(),
                    space: Some(SpaceSummary::from(&space)),
                    ..Default::default()
                };
                (text, out)
            }
            SpacesParams::List { all } => {
                let spaces = if all {
                    self.spaces.list_all(subject).await?
                } else {
                    self.projects
                        .list_mounted_spaces(subject, project_id)
                        .await?
                };

                let summaries: Vec<SpaceSummary> = spaces.iter().map(SpaceSummary::from).collect();
                let header = if all {
                    format!("All library spaces ({}):", summaries.len())
                } else {
                    format!("Mounted spaces ({}):", summaries.len())
                };
                let text = if summaries.is_empty() {
                    if all {
                        "No spaces in the library.".to_string()
                    } else {
                        "No spaces mounted on this project.".to_string()
                    }
                } else {
                    let lines: Vec<String> = summaries
                        .iter()
                        .map(|s| match &s.description {
                            Some(d) => format!("  - {} — {}", s.slug, d),
                            None => format!("  - {}", s.slug),
                        })
                        .collect();
                    format!("{header}\n{}", lines.join("\n"))
                };
                let out = SpacesOutput {
                    command: "list".to_string(),
                    spaces: Some(summaries),
                    ..Default::default()
                };
                (text, out)
            }
            SpacesParams::Search {
                slug,
                query,
                paths,
                limit,
            } => {
                let space = self.projects.space_for_subject(subject, &slug).await?;
                let path_prefixes = Self::normalize_path_prefixes(paths)?;
                let hits = self
                    .search
                    .search(
                        subject,
                        &[uuid::Uuid::from(space.id)],
                        &query,
                        &[SPACE_DOC_TYPE],
                        &path_prefixes,
                        limit,
                    )
                    .await?;

                let total = hits.len();
                let mut entries: Vec<String> = Vec::with_capacity(total);
                let mut results: Vec<SpaceSearchHit> = Vec::with_capacity(total);
                for hit in hits {
                    let preview: String = hit.fields.content.chars().take(200).collect();
                    let path = hit.fields.path.unwrap_or_default();
                    entries.push(format!(
                        "path: {path}\ntitle: {}\npreview: {preview}",
                        hit.fields.name,
                    ));
                    results.push(SpaceSearchHit {
                        doc_id: hit.fields.doc_id.to_string(),
                        title: hit.fields.name,
                        preview,
                        score: hit.score,
                        relative_path: (!path.is_empty()).then_some(path),
                    });
                }
                let text = if total == 0 {
                    format!("No matches in space '{}' for query.", space.slug)
                } else {
                    format!(
                        "Found {total} hit(s) in space '{}':\n\n{}",
                        space.slug,
                        entries.join("\n---\n"),
                    )
                };
                let out = SpacesOutput {
                    command: "search".to_string(),
                    space: Some(SpaceSummary::from(&space)),
                    results: Some(results),
                    ..Default::default()
                };
                (text, out)
            }
            SpacesParams::Draft => match self.changesets.open_draft_for(subject).await? {
                None => (
                    "No open draft.".to_string(),
                    SpacesOutput {
                        command: "draft".to_string(),
                        ..Default::default()
                    },
                ),
                Some(draft) => {
                    let view = self.changesets.status(subject, draft.id).await?;
                    let touched: Vec<TouchedFileOut> =
                        view.touched.iter().map(Into::into).collect();
                    let text = format!(
                        "Open draft: {} [{}] {}\n  commits: {}\n  mergeable: {}{}\n  touched: {} file(s)",
                        draft.id,
                        draft.branch(),
                        draft.title,
                        view.commits,
                        view.mergeable,
                        if view.mergeable {
                            String::new()
                        } else {
                            format!(" (conflicts: {})", view.conflicts.join(", "))
                        },
                        touched.len(),
                    );
                    let out = SpacesOutput {
                        command: "draft".to_string(),
                        draft: Some(ChangesetSummary::from(&draft)),
                        commits: Some(view.commits),
                        mergeable: Some(view.mergeable),
                        conflicts: (!view.conflicts.is_empty()).then_some(view.conflicts),
                        touched: Some(touched),
                        ..Default::default()
                    };
                    (text, out)
                }
            },
            SpacesParams::Drafts { status } => {
                let list = self.changesets.list(subject, status).await?;
                let summaries: Vec<ChangesetSummary> = list.iter().map(Into::into).collect();
                let text = if summaries.is_empty() {
                    "No changesets in this project.".to_string()
                } else {
                    let lines: Vec<String> = summaries
                        .iter()
                        .map(|c| format!("  - {} [{}] {}", c.id, c.status, c.title))
                        .collect();
                    format!("Changesets ({}):\n{}", summaries.len(), lines.join("\n"))
                };
                let out = SpacesOutput {
                    command: "drafts".to_string(),
                    drafts: Some(summaries),
                    ..Default::default()
                };
                (text, out)
            }
            SpacesParams::Publish { id } => {
                let id = self.resolve_draft_id(subject, id).await?;
                // rev2 D6: one verb, effect resolved by authority — not
                // a choice the caller makes.
                let can_land = subject
                    .can(AuthVerb::Update, AuthResource::Space(None))
                    .is_ok();
                let (text, out) = if can_land {
                    let (cs, merge_oid) = self.changesets.apply(subject, id).await?;
                    (
                        format!("Changeset {} landed as {merge_oid}.", cs.id),
                        SpacesOutput {
                            command: "publish".to_string(),
                            draft: Some(ChangesetSummary::from(&cs)),
                            outcome: Some("landed".to_string()),
                            merge_oid: Some(merge_oid),
                            ..Default::default()
                        },
                    )
                } else {
                    let cs = self.changesets.submit(subject, id).await?;
                    (
                        format!(
                            "Changeset {} submitted.\n  PR: {}",
                            cs.id,
                            cs.pr_url.as_deref().unwrap_or("(unknown)"),
                        ),
                        SpacesOutput {
                            command: "publish".to_string(),
                            pr_number: cs.pr_number,
                            pr_url: cs.pr_url.clone(),
                            draft: Some(ChangesetSummary::from(&cs)),
                            outcome: Some("pr_opened".to_string()),
                            ..Default::default()
                        },
                    )
                };
                (text, out)
            }
            SpacesParams::Discard { id, reason } => {
                let id = self.resolve_draft_id(subject, id).await?;
                let cs = self.changesets.discard(subject, id, reason).await?;
                let text = format!("Changeset {} discarded.", cs.id);
                let out = SpacesOutput {
                    command: "discard".to_string(),
                    draft: Some(ChangesetSummary::from(&cs)),
                    ..Default::default()
                };
                (text, out)
            }
            SpacesParams::Rebase { id } => {
                let id = self.resolve_draft_id(subject, id).await?;
                let cs = self.changesets.rebase(subject, id).await?;
                let text = format!(
                    "Changeset {} rebased.\n  base: {}\n  head: {}",
                    cs.id, cs.base_oid, cs.head_oid,
                );
                let out = SpacesOutput {
                    command: "rebase".to_string(),
                    draft: Some(ChangesetSummary::from(&cs)),
                    ..Default::default()
                };
                (text, out)
            }
        };

        Ok(SPACES_OUTPUT.success(text, &out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_search_minimal() {
        let json = serde_json::json!({
            "command": "search",
            "slug": "ops",
            "query": "deploy"
        });
        let params: SpacesParams = serde_json::from_value(json).unwrap();
        match params {
            SpacesParams::Search {
                slug,
                query,
                paths,
                limit,
            } => {
                assert_eq!(slug, "ops");
                assert_eq!(query, "deploy");
                assert!(paths.is_empty());
                assert_eq!(limit, default_search_limit());
            }
            _ => panic!("expected Search variant"),
        }
    }

    #[test]
    fn parse_search_with_limit() {
        let json = serde_json::json!({
            "command": "search",
            "slug": "ops",
            "query": "deploy",
            "limit": 25
        });
        let params: SpacesParams = serde_json::from_value(json).unwrap();
        match params {
            SpacesParams::Search { limit, .. } => assert_eq!(limit, 25),
            _ => panic!("expected Search variant"),
        }
    }

    #[test]
    fn parse_search_with_paths() {
        let json = serde_json::json!({
            "command": "search",
            "slug": "ops",
            "query": "deploy",
            "paths": ["triggers/", "runbooks/"]
        });
        let params: SpacesParams = serde_json::from_value(json).unwrap();
        match params {
            SpacesParams::Search { paths, .. } => {
                assert_eq!(
                    paths,
                    vec!["triggers/".to_string(), "runbooks/".to_string()]
                );
            }
            _ => panic!("expected Search variant"),
        }
    }

    #[test]
    fn search_command_name_matches_audit_action() {
        let p = SpacesParams::Search {
            slug: "ops".into(),
            query: "x".into(),
            paths: Vec::new(),
            limit: 1,
        };
        assert_eq!(p.command_name(), "search");
    }

    #[test]
    fn schema_advertises_search_command() {
        let schema = &*SPACES_SCHEMA;
        let cmd_enum = schema
            .pointer("/properties/command/enum")
            .expect("command.enum")
            .as_array()
            .expect("array");
        assert!(cmd_enum.iter().any(|v| v == "search"));
        assert!(schema.pointer("/properties/query").is_some());
        assert!(schema.pointer("/properties/limit").is_some());
        assert_eq!(
            schema
                .pointer("/properties/paths/type")
                .and_then(|v| v.as_str()),
            Some("array")
        );
        assert_eq!(
            schema
                .pointer("/properties/paths/items/type")
                .and_then(|v| v.as_str()),
            Some("string")
        );
    }

    #[test]
    fn normalize_paths_empty() {
        assert!(SpacesTool::normalize_path_prefixes(Vec::new())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn normalize_paths_drops_trailing_slash() {
        let got = SpacesTool::normalize_path_prefixes(vec!["triggers/".into()]).unwrap();
        assert_eq!(got, vec!["triggers".to_string()]);
    }

    #[test]
    fn normalize_paths_no_trailing_slash_kept() {
        let got = SpacesTool::normalize_path_prefixes(vec!["triggers".into()]).unwrap();
        assert_eq!(got, vec!["triggers".to_string()]);
    }

    #[test]
    fn normalize_paths_multiple_prefixes() {
        let got = SpacesTool::normalize_path_prefixes(vec!["triggers/".into(), "runbooks".into()])
            .unwrap();
        assert_eq!(got, vec!["triggers".to_string(), "runbooks".to_string()]);
    }

    #[test]
    fn normalize_paths_keeps_exact_file_path() {
        let got =
            SpacesTool::normalize_path_prefixes(vec!["triggers/account-locked.md".into()]).unwrap();
        assert_eq!(got, vec!["triggers/account-locked.md".to_string()]);
    }

    #[test]
    fn normalize_paths_silently_drops_empty_strings() {
        let got = SpacesTool::normalize_path_prefixes(vec!["".into(), "  ".into()]).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn normalize_paths_bare_slash_rejected_as_leading() {
        let err = SpacesTool::normalize_path_prefixes(vec!["/".into()]).unwrap_err();
        assert!(matches!(err, ToolSetsError::InvalidArgument(_)));
    }

    #[test]
    fn normalize_paths_rejects_leading_slash() {
        let err = SpacesTool::normalize_path_prefixes(vec!["/triggers".into()]).unwrap_err();
        assert!(matches!(err, ToolSetsError::InvalidArgument(_)));
    }

    #[test]
    fn normalize_paths_rejects_dotdot_segment() {
        let err = SpacesTool::normalize_path_prefixes(vec!["triggers/../etc".into()]).unwrap_err();
        assert!(matches!(err, ToolSetsError::InvalidArgument(_)));
    }

    #[test]
    fn normalize_paths_rejects_glob_metacharacters() {
        for raw in ["triggers/*.md", "trig?ers", "trig[ab]"] {
            let err = SpacesTool::normalize_path_prefixes(vec![raw.into()]).unwrap_err();
            assert!(
                matches!(err, ToolSetsError::InvalidArgument(_)),
                "expected InvalidArgument for {raw:?}",
            );
        }
    }
}
