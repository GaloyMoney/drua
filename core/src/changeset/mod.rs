pub mod entity;
pub mod error;
pub(crate) mod job;
pub mod repo;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tracing::instrument;

pub use entity::*;
pub use error::ChangesetError;
use repo::ChangesetRepo;

use drua_library::{ApplyOutcome, DraftObservation, Mergeability, RebaseOutcome};

use crate::agent::Agents;
use crate::audit::Audit;
use crate::auth::error::AuthorizationError;
use crate::auth::{AuthResource, AuthSubject, AuthVerb};
use crate::github_app::{GitHubAppTokenProvider, PullRequest};
use crate::primitives::*;

/// Cap on [`Changesets::touched_cache`]'s size. Entries are keyed by a
/// pair of immutable commit oids and never go stale, so eviction just
/// bounds memory — an arbitrary entry is dropped rather than the truly
/// least-recently-used one.
const TOUCHED_CACHE_CAP: usize = 1024;

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct ChangesetConfig {
    /// How often the GitHub PR-close poll runs. Default 60s.
    #[serde(default = "default_pr_poll_interval_secs")]
    pub pr_poll_interval_secs: u64,
}

impl Default for ChangesetConfig {
    fn default() -> Self {
        Self {
            pr_poll_interval_secs: default_pr_poll_interval_secs(),
        }
    }
}

fn default_pr_poll_interval_secs() -> u64 {
    60
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TouchedFile {
    pub space_slug: String,
    pub path: String,
    pub kind: TouchedKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchedKind {
    Added,
    Modified,
    Deleted,
}

impl From<drua_library::DeltaKind> for TouchedKind {
    fn from(kind: drua_library::DeltaKind) -> Self {
        match kind {
            drua_library::DeltaKind::Added => TouchedKind::Added,
            drua_library::DeltaKind::Modified => TouchedKind::Modified,
            drua_library::DeltaKind::Deleted => TouchedKind::Deleted,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChangesetStatusView {
    pub id: ChangesetId,
    pub title: String,
    pub status: ChangesetStatus,
    pub base_oid: String,
    pub head_oid: String,
    pub commits: usize,
    pub main_oid: String,
    pub mergeable: bool,
    pub conflicts: Vec<String>,
    pub touched: Arc<Vec<TouchedFile>>,
    pub pr_url: Option<String>,
}

/// What a `space:`/`draft:` path addresses, once path parsing and any
/// run-subject redirect have already settled on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceAddress {
    /// The published tree — `space:<slug>/...` outside a draft.
    Published,
    /// The caller's own open draft — `draft:<slug>/...`, or a run
    /// subject's `space:<slug>/...` (redirected upstream in `SpaceFs`).
    OwnDraft,
    /// An explicit `space:<slug>@<id>/...`.
    Changeset(ChangesetId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceIntent {
    Read,
    Write,
}

/// Everything about a draft a caller needs to render a stamp or decide
/// what happened, without touching git or Postgres again.
#[derive(Debug, Clone)]
pub struct DraftInfo {
    pub id: ChangesetId,
    pub status: ChangesetStatus,
    pub title: String,
    pub touched: usize,
    pub just_started: bool,
}

/// What a space read or write should be addressed against, plus the
/// draft it came from (if any) for stamp rendering.
pub struct ResolvedTarget {
    pub target: drua_library::SpaceTarget,
    pub draft: Option<DraftInfo>,
}

pub(crate) fn short_id(id: ChangesetId) -> String {
    id.to_string().chars().take(8).collect()
}

fn actor_for_subject(sub: &AuthSubject) -> Result<ChangesetActor, ChangesetError> {
    match sub {
        AuthSubject::Agent(_, agent_id, _)
        | AuthSubject::AgentOnBehalfOfUser(_, _, agent_id, _) => Ok(ChangesetActor::Agent {
            agent_id: *agent_id,
        }),
        AuthSubject::WorkflowExecutor(_, _, run_id, _) => {
            Ok(ChangesetActor::WorkflowRun { run_id: *run_id })
        }
        AuthSubject::User(user_id) | AuthSubject::ExportedAgent(user_id, _, _) => {
            Ok(ChangesetActor::User { user_id: *user_id })
        }
        AuthSubject::Anonymous => Err(ChangesetError::UnsupportedActor),
    }
}

type TouchedCacheKey = (String, String);

#[derive(Clone)]
pub struct Changesets {
    repo: ChangesetRepo,
    agents: Arc<Agents>,
    drafts: drua_library::Drafts,
    users: crate::user::Users,
    github: Option<Arc<GitHubAppTokenProvider>>,
    repo_coord: Option<(String, String)>,
    /// `(base_oid, head_oid) -> touched files`, so `changeset_target`
    /// (called on every draft read and write to render the touched-file
    /// count in the stamp) doesn't re-diff an unchanged draft each time.
    touched_cache: Arc<Mutex<HashMap<TouchedCacheKey, Arc<Vec<TouchedFile>>>>>,
}

impl Changesets {
    pub fn new(
        pool: &sqlx::PgPool,
        agents: &Arc<Agents>,
        library: &drua_library::Library,
        users: &crate::user::Users,
        github: Option<Arc<GitHubAppTokenProvider>>,
        repo_coord: Option<(String, String)>,
    ) -> Self {
        Self {
            repo: ChangesetRepo::new(pool),
            agents: Arc::clone(agents),
            drafts: library.drafts().clone(),
            users: users.clone(),
            github,
            repo_coord,
            touched_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn actor_key_for(&self, sub: &AuthSubject) -> Result<ChangesetActor, ChangesetError> {
        if let AuthSubject::Agent(_, agent_id, _)
        | AuthSubject::AgentOnBehalfOfUser(_, _, agent_id, _) = sub
        {
            match self.agents.find_by_id_unchecked(*agent_id).await {
                Ok(agent) => {
                    if let Some(run_id) = agent.workflow_run_id {
                        return Ok(ChangesetActor::WorkflowRun { run_id });
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        agent_id = %agent_id,
                        "actor_key_for: agent lookup failed; keying the draft by agent id directly"
                    );
                }
            }
        }
        actor_for_subject(sub)
    }

    #[instrument(name = "domain.changeset.open_draft_for", skip(self, sub))]
    pub async fn open_draft_for(
        &self,
        sub: &AuthSubject,
    ) -> Result<Option<Changeset>, ChangesetError> {
        let actor = self.actor_key_for(sub).await?;
        self.find_open_for_actor(actor).await
    }

    pub async fn open_draft_for_run(
        &self,
        run_id: WorkflowRunId,
    ) -> Result<Option<Changeset>, ChangesetError> {
        self.find_open_for_actor(ChangesetActor::WorkflowRun { run_id })
            .await
    }

    async fn find_open_for_actor(
        &self,
        actor: ChangesetActor,
    ) -> Result<Option<Changeset>, ChangesetError> {
        let page = self
            .repo
            .list_for_opened_by_actor_by_created_at(
                actor.to_string(),
                es_entity::PaginatedQueryArgs {
                    first: 1,
                    after: None,
                },
                es_entity::ListDirection::Descending,
            )
            .await?;
        Ok(page.entities.into_iter().next().filter(|cs| cs.is_open()))
    }

    #[instrument(name = "domain.changeset.draft_for", skip(self, sub, description))]
    pub async fn draft_for(
        &self,
        sub: &AuthSubject,
        title: Option<String>,
        description: Option<String>,
        first_touched_path: Option<&str>,
    ) -> Result<Changeset, ChangesetError> {
        sub.require_space_drafting()?;
        let actor = self.actor_key_for(sub).await?;
        if let Some(cs) = self.find_open_for_actor(actor).await? {
            return Ok(cs);
        }

        let project_id = sub.effective_project_id();
        let base_oid = self.drafts.fresh_base().await?;
        let title = match title {
            Some(t) => t,
            None => self.derive_draft_title(actor, first_touched_path).await,
        };

        let mut builder = NewChangeset::builder()
            .project_id(project_id)
            .title(title)
            .base_oid(base_oid.clone())
            .opened_by(actor);
        if let Some(desc) = description {
            builder = builder.description(desc);
        }
        let new_changeset = builder
            .build()
            .expect("all required NewChangeset fields set");

        let mut op = self.repo.begin_op().await?;
        let changeset = match self.repo.create_in_op(&mut op, new_changeset).await {
            Ok(cs) => cs,
            Err(e) if e.was_duplicate() => {
                drop(op);
                return self
                    .find_open_for_actor(actor)
                    .await?
                    .ok_or_else(|| ChangesetError::from(e));
            }
            Err(e) => return Err(e.into()),
        };
        op.commit().await?;

        if let Err(e) = self.drafts.open(changeset.draft_name(), &base_oid).await {
            tracing::warn!(
                error = %e,
                changeset_id = %changeset.id,
                "changeset.draft_for: open failed; ensure_ref will repair it on first use"
            );
        }

        Audit::record_action_if_unset("changeset.draft_for");
        if let Some(project_id) = project_id {
            Audit::record_project_id(project_id);
        }
        Audit::record_changeset_id(changeset.id);

        Ok(changeset)
    }

    async fn derive_draft_title(
        &self,
        actor: ChangesetActor,
        first_touched_path: Option<&str>,
    ) -> String {
        let who = match actor {
            ChangesetActor::Agent { agent_id } => {
                match self.agents.find_by_id_unchecked(agent_id).await {
                    Ok(agent) => agent.name,
                    Err(_) => describe_actor(&actor),
                }
            }
            ChangesetActor::User { user_id } => match self.users.find_by_id(user_id).await {
                Ok(user) => user.email.unwrap_or_else(|| describe_actor(&actor)),
                Err(_) => describe_actor(&actor),
            },
            ChangesetActor::WorkflowRun { .. } => describe_actor(&actor),
        };
        match first_touched_path {
            Some(path) if !path.is_empty() => format!("Draft by {who} — {path}"),
            _ => format!("Draft by {who}"),
        }
    }

    #[instrument(name = "domain.changeset.find_for_target", skip(self, sub))]
    pub(crate) async fn find_for_target(
        &self,
        sub: &AuthSubject,
        id: ChangesetId,
    ) -> Result<Changeset, ChangesetError> {
        let cs = self.repo.find_by_id(id).await?;
        self.check_same_project(sub, &cs)?;
        Ok(cs)
    }

    pub(crate) async fn ensure_ref(
        &self,
        cs: &Changeset,
    ) -> Result<drua_library::DraftHandle, ChangesetError> {
        Ok(self.drafts.handle(cs.draft_name(), &cs.head_oid).await?)
    }

    pub(crate) async fn record_commit(
        &self,
        id: ChangesetId,
        head_oid: String,
        action: &str,
        path: &str,
    ) -> Result<(), ChangesetError> {
        let mut op = self.repo.begin_op().await?;
        let mut cs = self.repo.find_by_id_in_op(&mut op, id).await?;
        if cs.record_commit(head_oid, action, path)?.did_execute() {
            self.repo.update_in_op(&mut op, &mut cs).await?;
        }
        op.commit().await?;
        Ok(())
    }

    /// Resolves what a `space:`/`draft:` read or write should be
    /// addressed against, applying draft policy: whether this subject
    /// may write at all, whether an explicit changeset is open, whether
    /// an own-draft write opens a new one, and whether a direct write
    /// to the published tree is blocked by an open draft.
    #[instrument(name = "domain.changeset.target_for", skip(self, sub))]
    pub async fn target_for(
        &self,
        sub: &AuthSubject,
        slug: &str,
        address: SpaceAddress,
        intent: SpaceIntent,
        first_touched_path: Option<&str>,
    ) -> Result<ResolvedTarget, ChangesetError> {
        match address {
            SpaceAddress::Changeset(id) => {
                let cs = self.find_for_target(sub, id).await?;
                if intent == SpaceIntent::Write {
                    if !sub.can_draft_spaces() {
                        return Err(ChangesetError::ReadOnly {
                            slug: slug.to_string(),
                        });
                    }
                    if !cs.is_open() {
                        return Err(ChangesetError::ChangesetNotOpen {
                            id: cs.id.to_string(),
                            status: format!("{:?}", cs.status),
                        });
                    }
                }
                self.resolved_for_changeset(cs, false).await
            }
            SpaceAddress::OwnDraft => {
                if intent == SpaceIntent::Read {
                    if !sub.can_draft_spaces() {
                        return Err(ChangesetError::ReadOnly {
                            slug: slug.to_string(),
                        });
                    }
                    return match self.open_draft_for(sub).await? {
                        Some(cs) => self.resolved_for_changeset(cs, false).await,
                        None => Ok(ResolvedTarget {
                            target: drua_library::SpaceTarget::Main,
                            draft: None,
                        }),
                    };
                }
                if !sub.can_draft_spaces() {
                    return Err(ChangesetError::ReadOnly {
                        slug: slug.to_string(),
                    });
                }
                match self.open_draft_for(sub).await? {
                    Some(cs) => self.resolved_for_changeset(cs, false).await,
                    None if sub.in_workflow_run() => Err(ChangesetError::RunReadOnly {
                        slug: slug.to_string(),
                    }),
                    None => {
                        let cs = self.draft_for(sub, None, None, first_touched_path).await?;
                        self.resolved_for_changeset(cs, true).await
                    }
                }
            }
            SpaceAddress::Published => {
                if intent != SpaceIntent::Write {
                    return Ok(ResolvedTarget {
                        target: drua_library::SpaceTarget::Main,
                        draft: None,
                    });
                }
                if !sub.can_draft_spaces() {
                    return Err(ChangesetError::ReadOnly {
                        slug: slug.to_string(),
                    });
                }
                match self.open_draft_for(sub).await? {
                    Some(cs) => Err(ChangesetError::DraftOpen {
                        id: short_id(cs.id),
                        title: cs.title,
                        slug: slug.to_string(),
                    }),
                    None => Ok(ResolvedTarget {
                        target: drua_library::SpaceTarget::Main,
                        draft: None,
                    }),
                }
            }
        }
    }

    async fn resolved_for_changeset(
        &self,
        cs: Changeset,
        just_started: bool,
    ) -> Result<ResolvedTarget, ChangesetError> {
        // ensure_ref first: it recovers from a not-yet-fetched base commit,
        // so touched_count's diff runs once that recovery has had a
        // chance to bring the objects in.
        let handle = self.ensure_ref(&cs).await?;
        let touched = self.touched_count(&cs).await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, changeset_id = %cs.id, "resolved_for_changeset: touched_count failed; reporting 0");
            0
        });
        Ok(ResolvedTarget {
            target: drua_library::SpaceTarget::Draft(handle),
            draft: Some(DraftInfo {
                id: cs.id,
                status: cs.status,
                title: cs.title,
                touched,
                just_started,
            }),
        })
    }

    /// Records a write against `id`'s draft and returns its refreshed
    /// [`DraftInfo`] — the touched-file count reflects `head_oid`.
    #[instrument(name = "domain.changeset.commit_recorded", skip(self))]
    pub async fn commit_recorded(
        &self,
        id: ChangesetId,
        head_oid: String,
        action: &str,
        path: &str,
    ) -> Result<DraftInfo, ChangesetError> {
        self.record_commit(id, head_oid, action, path).await?;
        let cs = self.repo.find_by_id(id).await?;
        let touched = self.touched_count(&cs).await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, changeset_id = %cs.id, "commit_recorded: touched_count failed; reporting 0");
            0
        });
        Ok(DraftInfo {
            id: cs.id,
            status: cs.status,
            title: cs.title,
            touched,
            just_started: false,
        })
    }

    #[instrument(name = "domain.changeset.list", skip(self, sub))]
    pub async fn list(
        &self,
        sub: &AuthSubject,
        status: Option<ChangesetStatus>,
        project_id: Option<ProjectId>,
    ) -> Result<Vec<Changeset>, ChangesetError> {
        if !sub.is_admin() {
            return Err(AuthorizationError::Forbidden {
                verb: AuthVerb::Update,
                resource: AuthResource::Space(None),
            }
            .into());
        }
        let mut out = match project_id {
            Some(pid) => self.list_all_for_project(pid).await?,
            None => self.list_all_unfiltered().await?,
        };
        if let Some(status) = status {
            out.retain(|cs| cs.status == status);
        }
        out.sort_by_key(|b| std::cmp::Reverse(b.created_at()));
        Ok(out)
    }

    async fn list_all_for_project(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<Changeset>, ChangesetError> {
        let mut out = Vec::new();
        let mut after = None;
        loop {
            let page = self
                .repo
                .list_for_project_id_by_created_at(
                    Some(project_id),
                    es_entity::PaginatedQueryArgs { first: 200, after },
                    es_entity::ListDirection::Descending,
                )
                .await?;
            out.extend(page.entities);
            if !page.has_next_page {
                break;
            }
            after = page.end_cursor;
        }
        Ok(out)
    }

    async fn list_all_unfiltered(&self) -> Result<Vec<Changeset>, ChangesetError> {
        let mut out = Vec::new();
        let mut after = None;
        loop {
            let page = self
                .repo
                .list_by_created_at(
                    es_entity::PaginatedQueryArgs { first: 200, after },
                    es_entity::ListDirection::Descending,
                )
                .await?;
            out.extend(page.entities);
            if !page.has_next_page {
                break;
            }
            after = page.end_cursor;
        }
        Ok(out)
    }

    #[instrument(name = "domain.changeset.status", skip(self, sub))]
    pub async fn status(
        &self,
        sub: &AuthSubject,
        id: ChangesetId,
    ) -> Result<ChangesetStatusView, ChangesetError> {
        let cs = self.repo.find_by_id(id).await?;
        self.check_same_project(sub, &cs)?;

        let main_oid = self.drafts.current_main().await?;
        let (mergeable, conflicts) =
            match self.drafts.mergeability(&cs.base_oid, &cs.head_oid).await? {
                Mergeability::Clean => (true, Vec::new()),
                Mergeability::Conflicts(paths) => (false, paths),
            };
        let touched = self.touched_files(&cs).await?;

        Ok(ChangesetStatusView {
            id: cs.id,
            title: cs.title.clone(),
            status: cs.status,
            base_oid: cs.base_oid.clone(),
            head_oid: cs.head_oid.clone(),
            commits: cs.commit_count(),
            main_oid,
            mergeable,
            conflicts,
            touched,
            pr_url: cs.pr_url.clone(),
        })
    }

    #[instrument(name = "domain.changeset.discard", skip(self, sub))]
    pub async fn discard(
        &self,
        sub: &AuthSubject,
        id: ChangesetId,
        reason: Option<String>,
    ) -> Result<Changeset, ChangesetError> {
        let mut op = self.repo.begin_op().await?;
        let mut cs = self.repo.find_by_id_in_op(&mut op, id).await?;
        self.check_same_project(sub, &cs)?;
        self.check_discard_authority(sub, &cs).await?;

        if cs.discard(reason)?.did_execute() {
            self.repo.update_in_op(&mut op, &mut cs).await?;
        }
        op.commit().await?;

        if let Err(e) = self.drafts.close(cs.draft_name()).await {
            tracing::warn!(
                error = %e,
                changeset_id = %id,
                "changeset.discard: delete_ref failed (best effort; branch may already be gone)"
            );
        }

        Audit::record_action_if_unset("changeset.discard");
        Audit::record_changeset_id(id);
        Ok(cs)
    }

    #[instrument(name = "domain.changeset.submit", skip(self, sub, title, body))]
    pub async fn submit(
        &self,
        sub: &AuthSubject,
        id: ChangesetId,
        title: String,
        body: String,
    ) -> Result<Changeset, ChangesetError> {
        sub.require_space_drafting()?;
        let mut op = self.repo.begin_op().await?;
        let mut cs = self.repo.find_by_id_in_op(&mut op, id).await?;
        self.check_same_project(sub, &cs)?;
        self.check_owner_or_admin(sub, &cs, "submit").await?;
        if !cs.is_open() {
            return Err(ChangesetError::InvalidTransition {
                from: cs.status,
                op: "submit",
            });
        }
        if !cs.has_commits() {
            return Err(ChangesetError::Empty { id });
        }

        match self.drafts.mergeability(&cs.base_oid, &cs.head_oid).await? {
            Mergeability::Clean => {}
            Mergeability::Conflicts(paths) => return Err(ChangesetError::Conflicts { id, paths }),
        }

        let (owner, repo) = self
            .repo_coord
            .clone()
            .ok_or(ChangesetError::PrUnavailable)?;
        let github = self.github.as_ref().ok_or(ChangesetError::PrUnavailable)?;
        let full_body = append_pr_trailers(&body, &cs);
        let pr = github
            .create_pull(&owner, &repo, &cs.branch(), "main", &title, &full_body)
            .await?;

        let head_oid = cs.head_oid.clone();
        if cs
            .submit(head_oid, pr.number, pr.html_url.clone(), title, body)?
            .did_execute()
        {
            self.repo.update_in_op(&mut op, &mut cs).await?;
        }
        op.commit().await?;

        Audit::record_action_if_unset("changeset.submit");
        Audit::record_changeset_id(id);
        Ok(cs)
    }

    #[instrument(name = "domain.changeset.apply", skip(self, sub))]
    pub async fn apply(
        &self,
        sub: &AuthSubject,
        id: ChangesetId,
        title: Option<String>,
        body: Option<String>,
    ) -> Result<(Changeset, String), ChangesetError> {
        sub.require_space_drafting()?;
        let mut op = self.repo.begin_op().await?;
        let mut cs = self.repo.find_by_id_in_op(&mut op, id).await?;
        self.check_same_project(sub, &cs)?;
        self.check_owner_or_admin(sub, &cs, "apply").await?;
        if !matches!(
            cs.status,
            ChangesetStatus::Open | ChangesetStatus::Submitted
        ) {
            return Err(ChangesetError::InvalidTransition {
                from: cs.status,
                op: "apply",
            });
        }
        if !cs.has_commits() {
            return Err(ChangesetError::Empty { id });
        }

        let actor = actor_for_subject(sub)?;
        let attribution = self.users.commit_attribution().await;
        let message = format!(
            "{}\n\n{}",
            title
                .clone()
                .unwrap_or_else(|| format!("changeset: {}", cs.title)),
            body.clone()
                .or_else(|| cs.description.clone())
                .unwrap_or_default(),
        );
        let merge_oid = match self
            .drafts
            .apply(&cs.head_oid, message, attribution)
            .await?
        {
            ApplyOutcome::Merged { merge_oid } => merge_oid,
            ApplyOutcome::Conflicts(paths) => return Err(ChangesetError::Conflicts { id, paths }),
        };

        let pr_number = cs.pr_number;
        if cs
            .apply(merge_oid.clone(), actor, title, body)?
            .did_execute()
        {
            self.repo.update_in_op(&mut op, &mut cs).await?;
        }
        op.commit().await?;

        if let (Some(pr_number), Some(github), Some((owner, repo))) =
            (pr_number, self.github.as_ref(), self.repo_coord.clone())
        {
            if let Err(e) = github
                .close_pull(
                    &owner,
                    &repo,
                    pr_number,
                    Some(&format!("Applied by drua as {merge_oid}")),
                )
                .await
            {
                tracing::warn!(
                    error = %e,
                    changeset_id = %id,
                    "changeset.apply: failed to close PR (best effort; the merge itself already landed)"
                );
            }
        }
        if let Err(e) = self.drafts.close(cs.draft_name()).await {
            tracing::warn!(
                error = %e,
                changeset_id = %id,
                "changeset.apply: delete_ref failed (best effort; branch may already be gone)"
            );
        }

        Audit::record_action_if_unset("changeset.apply");
        Audit::record_changeset_id(id);
        Ok((cs, merge_oid))
    }

    #[instrument(name = "domain.changeset.rebase", skip(self, sub))]
    pub async fn rebase(
        &self,
        sub: &AuthSubject,
        id: ChangesetId,
    ) -> Result<Changeset, ChangesetError> {
        let mut op = self.repo.begin_op().await?;
        let mut cs = self.repo.find_by_id_in_op(&mut op, id).await?;
        self.check_same_project(sub, &cs)?;
        self.check_owner_or_admin(sub, &cs, "rebase").await?;
        if !cs.is_open() {
            return Err(ChangesetError::InvalidTransition {
                from: cs.status,
                op: "rebase",
            });
        }

        let attribution = self.users.commit_attribution().await;
        let message = format!("changeset: {} (rebased)", cs.title);
        match self
            .drafts
            .rebase(cs.draft_name(), &cs.head_oid, message, attribution)
            .await?
        {
            RebaseOutcome::Rebased { base_oid, head_oid } => {
                if cs.rebase(base_oid, head_oid)?.did_execute() {
                    self.repo.update_in_op(&mut op, &mut cs).await?;
                }
                op.commit().await?;
                Audit::record_action_if_unset("changeset.rebase");
                Audit::record_changeset_id(id);
                Ok(cs)
            }
            RebaseOutcome::Conflicts(paths) => Err(ChangesetError::Conflicts { id, paths }),
        }
    }

    #[instrument(name = "domain.changeset.observe_main", skip(self))]
    pub(crate) async fn observe_main(&self, main_oid: &str) -> Result<(), ChangesetError> {
        for status in [ChangesetStatus::Open, ChangesetStatus::Submitted] {
            let mut after = None;
            loop {
                let page = self
                    .repo
                    .list_for_status_by_created_at(
                        status,
                        es_entity::PaginatedQueryArgs { first: 200, after },
                        es_entity::ListDirection::Ascending,
                    )
                    .await?;
                for cs in &page.entities {
                    if let Err(e) = self.observe_one(cs, main_oid, status).await {
                        tracing::warn!(
                            error = %e,
                            changeset_id = %cs.id,
                            "observe_main: failed to observe changeset; will retry on the next tick"
                        );
                    }
                }
                if !page.has_next_page {
                    break;
                }
                after = page.end_cursor;
            }
        }
        Ok(())
    }

    async fn observe_one(
        &self,
        cs: &Changeset,
        main_oid: &str,
        status: ChangesetStatus,
    ) -> Result<(), ChangesetError> {
        match self
            .drafts
            .observe(cs.draft_name(), &cs.base_oid, &cs.head_oid, main_oid)
            .await?
        {
            DraftObservation::MergedInto { main_oid } => {
                self.mark_merged_in_op(cs.id, &main_oid).await
            }
            DraftObservation::Advanced { tip } => {
                self.record_commit(cs.id, tip, "external", "").await
            }
            // The PR poll reads GitHub directly and will mark this
            // Rejected (or Merged, for a squash/rebase merge) on its own
            // schedule. Racing it here with a git-only guess risks
            // Abandoned winning over the correct outcome.
            DraftObservation::Missing
                if status == ChangesetStatus::Submitted
                    && !poll_owns_missing_ref(self.github.is_some(), cs.pr_number) =>
            {
                self.mark_abandoned_in_op(cs.id).await
            }
            DraftObservation::Missing | DraftObservation::Unchanged => Ok(()),
        }
    }

    async fn mark_merged_in_op(
        &self,
        id: ChangesetId,
        merge_oid: &str,
    ) -> Result<(), ChangesetError> {
        let mut op = self.repo.begin_op().await?;
        let mut cs = self.repo.find_by_id_in_op(&mut op, id).await?;
        if cs.mark_merged(merge_oid.to_string())?.did_execute() {
            self.repo.update_in_op(&mut op, &mut cs).await?;
        }
        op.commit().await?;

        if let Err(e) = self.drafts.close(cs.draft_name()).await {
            tracing::warn!(
                error = %e,
                changeset_id = %id,
                "observe_main: delete_ref after merge failed (best effort; branch may already be gone)"
            );
        }
        Audit::record_action_if_unset("changeset.observe_merged");
        Audit::record_changeset_id(id);
        Ok(())
    }

    /// Applies GitHub's PR state to a Submitted changeset: merged (any merge
    /// method) becomes Merged, closed-unmerged becomes Rejected, open is a
    /// no-op. Free of any polling detail — callable directly from a webhook
    /// handler in a later iteration, as well as from the poll job.
    #[instrument(name = "domain.changeset.reconcile_pr_state", skip(self, pr))]
    pub async fn reconcile_pr_state(
        &self,
        id: ChangesetId,
        pr: &PullRequest,
    ) -> Result<(), ChangesetError> {
        let result = if pr.merged {
            match pr.merge_commit_sha.as_deref() {
                Some(merge_oid) => self.mark_merged_in_op(id, merge_oid).await,
                None => {
                    tracing::warn!(
                        changeset_id = %id,
                        pr_number = pr.number,
                        "reconcile_pr_state: PR reports merged with no merge_commit_sha; skipping"
                    );
                    return Ok(());
                }
            }
        } else if pr.state == "closed" {
            self.mark_rejected_in_op(id, pr.number).await
        } else {
            return Ok(());
        };

        match result {
            Ok(()) => Ok(()),
            // Another path (a direct git merge, `apply`) already finished
            // this changeset first; the poll just lost the race.
            Err(ChangesetError::InvalidTransition { from, op }) => {
                tracing::debug!(
                    changeset_id = %id,
                    ?from,
                    op,
                    "reconcile_pr_state: changeset already finished via another path"
                );
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Every Submitted changeset with a `pr_number` attached, across every
    /// project — the poll job's work list.
    pub(crate) async fn list_submitted_with_pr(&self) -> Result<Vec<Changeset>, ChangesetError> {
        let mut out = Vec::new();
        let mut after = None;
        loop {
            let page = self
                .repo
                .list_for_status_by_created_at(
                    ChangesetStatus::Submitted,
                    es_entity::PaginatedQueryArgs { first: 200, after },
                    es_entity::ListDirection::Ascending,
                )
                .await?;
            out.extend(
                page.entities
                    .into_iter()
                    .filter(|cs| cs.pr_number.is_some()),
            );
            if !page.has_next_page {
                break;
            }
            after = page.end_cursor;
        }
        Ok(out)
    }

    async fn mark_rejected_in_op(
        &self,
        id: ChangesetId,
        pr_number: u64,
    ) -> Result<(), ChangesetError> {
        let mut op = self.repo.begin_op().await?;
        let mut cs = self.repo.find_by_id_in_op(&mut op, id).await?;
        if cs.mark_rejected(pr_number)?.did_execute() {
            self.repo.update_in_op(&mut op, &mut cs).await?;
        }
        op.commit().await?;

        if let Err(e) = self.drafts.close(cs.draft_name()).await {
            tracing::warn!(
                error = %e,
                changeset_id = %id,
                "reconcile_pr_state: delete_ref after rejection failed (best effort; branch may already be gone)"
            );
        }
        Audit::record_action_if_unset("changeset.observe_rejected");
        Audit::record_changeset_id(id);
        Ok(())
    }

    /// Deletes the ref of every finished (terminal-status) changeset still
    /// found among the library's draft refs. Driven by the refs that
    /// exist, rather than by scanning every finished changeset — the
    /// backstop for the best-effort `delete_ref` calls in
    /// `mark_rejected_in_op`, `mark_merged_in_op`, `discard` and `apply`,
    /// any of which can leave a ref behind if the delete itself fails.
    #[instrument(name = "domain.changeset.sweep_finished_refs", skip(self))]
    pub async fn sweep_finished_refs(&self) -> Result<(), ChangesetError> {
        let names = self.drafts.list().await?;
        let mut deleted = 0usize;
        for name in names {
            let id = ChangesetId::from(name.uuid());
            let cs = match self.repo.find_by_id(id).await {
                Ok(cs) => cs,
                Err(e) if e.was_not_found() => {
                    tracing::warn!(
                        changeset_id = %id,
                        "sweep_finished_refs: no changeset row for this ref; leaving it alone"
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        changeset_id = %id,
                        "sweep_finished_refs: lookup failed; will retry next tick"
                    );
                    continue;
                }
            };
            if !cs.status.is_terminal() {
                continue;
            }
            if let Err(e) = self.drafts.close(name).await {
                tracing::warn!(
                    error = %e,
                    changeset_id = %id,
                    "sweep_finished_refs: delete_ref failed; will retry next tick"
                );
                continue;
            }
            deleted += 1;
        }
        if deleted > 0 {
            tracing::info!(
                deleted,
                "sweep_finished_refs: deleted leftover refs of finished changesets"
            );
        }
        Ok(())
    }

    async fn mark_abandoned_in_op(&self, id: ChangesetId) -> Result<(), ChangesetError> {
        let mut op = self.repo.begin_op().await?;
        let mut cs = self.repo.find_by_id_in_op(&mut op, id).await?;
        if cs.mark_abandoned()?.did_execute() {
            self.repo.update_in_op(&mut op, &mut cs).await?;
        }
        op.commit().await?;

        Audit::record_action_if_unset("changeset.observe_abandoned");
        Audit::record_changeset_id(id);
        Ok(())
    }

    fn check_same_project(&self, sub: &AuthSubject, cs: &Changeset) -> Result<(), ChangesetError> {
        match (sub.effective_project_id(), cs.project_id) {
            (Some(pid), Some(cpid)) if pid == cpid => Ok(()),
            (Some(_), Some(_)) => Err(ChangesetError::Foreign { id: cs.id }),
            (Some(_), None) => Ok(()),
            (None, _) => Ok(()),
        }
    }

    async fn check_discard_authority(
        &self,
        sub: &AuthSubject,
        cs: &Changeset,
    ) -> Result<(), ChangesetError> {
        if sub.is_admin() {
            return Ok(());
        }
        if cs.status == ChangesetStatus::Open {
            if let Ok(actor) = self.actor_key_for(sub).await {
                if cs.opened_by == actor {
                    return Ok(());
                }
            }
        }
        Err(ChangesetError::Forbidden {
            id: cs.id,
            action: "discard",
        })
    }

    async fn check_owner_or_admin(
        &self,
        sub: &AuthSubject,
        cs: &Changeset,
        action: &'static str,
    ) -> Result<(), ChangesetError> {
        if sub.is_admin() {
            return Ok(());
        }
        if let Ok(actor) = self.actor_key_for(sub).await {
            if cs.opened_by == actor {
                return Ok(());
            }
        }
        Err(ChangesetError::Forbidden { id: cs.id, action })
    }

    pub(crate) async fn touched_count(&self, cs: &Changeset) -> Result<usize, ChangesetError> {
        Ok(self.touched_files(cs).await?.len())
    }

    pub(crate) async fn touched_paths_in(
        &self,
        cs: &Changeset,
        slug: &str,
    ) -> Result<Vec<String>, ChangesetError> {
        Ok(self
            .touched_files(cs)
            .await?
            .iter()
            .filter(|t| t.space_slug == slug)
            .map(|t| t.path.clone())
            .collect())
    }

    async fn touched_files(&self, cs: &Changeset) -> Result<Arc<Vec<TouchedFile>>, ChangesetError> {
        let key = (cs.base_oid.clone(), cs.head_oid.clone());
        if let Some(cached) = self
            .touched_cache
            .lock()
            .expect("touched cache lock poisoned")
            .get(&key)
        {
            return Ok(Arc::clone(cached));
        }

        let touched = self.drafts.touched(&cs.base_oid, &cs.head_oid).await?;
        let out: Vec<TouchedFile> = touched
            .into_iter()
            .map(|t| TouchedFile {
                space_slug: t.space_slug,
                path: t.rel_path,
                kind: t.kind.into(),
            })
            .collect();
        let out = Arc::new(out);

        let mut guard = self
            .touched_cache
            .lock()
            .expect("touched cache lock poisoned");
        if guard.len() >= TOUCHED_CACHE_CAP {
            if let Some(evict) = guard.keys().next().cloned() {
                guard.remove(&evict);
            }
        }
        guard.insert(key, Arc::clone(&out));
        Ok(out)
    }
}

/// Whether a missing changeset ref should be left for the PR poll to
/// resolve (via [`Changesets::reconcile_pr_state`]) rather than guessed
/// at here as Abandoned. True only when both a GitHub App is configured
/// and this changeset actually has a PR to poll — otherwise (e.g. a
/// `file://` remote in tests) the git-only guess is the only signal
/// available and must run as before.
fn poll_owns_missing_ref(has_github_app: bool, pr_number: Option<u64>) -> bool {
    has_github_app && pr_number.is_some()
}

fn describe_actor(actor: &ChangesetActor) -> String {
    match actor {
        ChangesetActor::Agent { agent_id } => format!("agent {agent_id}"),
        ChangesetActor::WorkflowRun { run_id } => format!("workflow run {run_id}"),
        ChangesetActor::User { user_id } => format!("user {user_id}"),
    }
}

fn append_pr_trailers(body: &str, cs: &Changeset) -> String {
    let mut out = body.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("\nDrua-Changeset: {}\n", cs.id));
    if let Some(run_id) = cs.opened_by.workflow_run_id() {
        out.push_str(&format!("Drua-Workflow-Run: {run_id}\n"));
    }
    if let ChangesetActor::User { user_id } = &cs.opened_by {
        out.push_str(&format!("Drua-Acting-User: {user_id}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};

    use super::*;

    fn open_changeset() -> Changeset {
        let new = NewChangeset::builder()
            .project_id(Some(ProjectId::new()))
            .title("curate: relink notes")
            .description("moves stale notes into the new effort")
            .base_oid("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .opened_by(ChangesetActor::Agent {
                agent_id: AgentId::new(),
            })
            .build()
            .unwrap();
        Changeset::try_from_events(new.into_events()).unwrap()
    }

    #[test]
    fn describe_actor_names_each_variant() {
        let agent_id = AgentId::new();
        assert_eq!(
            describe_actor(&ChangesetActor::Agent { agent_id }),
            format!("agent {agent_id}")
        );
        let run_id = WorkflowRunId::new();
        assert_eq!(
            describe_actor(&ChangesetActor::WorkflowRun { run_id }),
            format!("workflow run {run_id}")
        );
        let user_id = UserId::new();
        assert_eq!(
            describe_actor(&ChangesetActor::User { user_id }),
            format!("user {user_id}")
        );
    }

    #[test]
    fn append_pr_trailers_preserves_the_callers_body_and_adds_the_changeset_trailer() {
        let cs = open_changeset();
        let out = append_pr_trailers("my own PR description", &cs);
        assert!(out.starts_with("my own PR description"));
        assert!(out.contains(&format!("Drua-Changeset: {}", cs.id)));
    }

    #[test]
    fn append_pr_trailers_adds_workflow_run_and_acting_user_trailers() {
        let run_id = WorkflowRunId::new();
        let new = NewChangeset::builder()
            .project_id(Some(ProjectId::new()))
            .title("t")
            .base_oid("a")
            .opened_by(ChangesetActor::WorkflowRun { run_id })
            .build()
            .unwrap();
        let cs = Changeset::try_from_events(new.into_events()).unwrap();
        let out = append_pr_trailers("body", &cs);
        assert!(out.contains(&format!("Drua-Workflow-Run: {run_id}")));

        let user_id = UserId::new();
        let new = NewChangeset::builder()
            .project_id(Some(ProjectId::new()))
            .title("t")
            .base_oid("a")
            .opened_by(ChangesetActor::User { user_id })
            .build()
            .unwrap();
        let cs = Changeset::try_from_events(new.into_events()).unwrap();
        let out = append_pr_trailers("body", &cs);
        assert!(out.contains(&format!("Drua-Acting-User: {user_id}")));
    }

    #[test]
    fn poll_owns_missing_ref_only_when_github_app_and_pr_number_both_present() {
        assert!(poll_owns_missing_ref(true, Some(1)));
        assert!(!poll_owns_missing_ref(true, None));
        assert!(!poll_owns_missing_ref(false, Some(1)));
        assert!(!poll_owns_missing_ref(false, None));
    }

    #[test]
    fn touched_kind_conversion_matches_delta_kind() {
        assert_eq!(
            TouchedKind::from(drua_library::DeltaKind::Added),
            TouchedKind::Added
        );
        assert_eq!(
            TouchedKind::from(drua_library::DeltaKind::Modified),
            TouchedKind::Modified
        );
        assert_eq!(
            TouchedKind::from(drua_library::DeltaKind::Deleted),
            TouchedKind::Deleted
        );
    }

    #[test]
    fn actor_for_subject_maps_known_subjects() {
        let agent_id = AgentId::new();
        let project_id = ProjectId::new();
        let sub = AuthSubject::Agent(project_id, agent_id, Vec::new());
        assert_eq!(
            actor_for_subject(&sub).unwrap(),
            ChangesetActor::Agent { agent_id }
        );

        let run_id = WorkflowRunId::new();
        let def_id = WorkflowDefinitionId::new();
        let sub = AuthSubject::WorkflowExecutor(project_id, def_id, run_id, Vec::new());
        assert_eq!(
            actor_for_subject(&sub).unwrap(),
            ChangesetActor::WorkflowRun { run_id }
        );

        let user_id = UserId::new();
        let sub = AuthSubject::User(user_id);
        assert_eq!(
            actor_for_subject(&sub).unwrap(),
            ChangesetActor::User { user_id }
        );
    }

    #[test]
    fn actor_for_subject_maps_exported_agent_to_user() {
        let user_id = UserId::new();
        let sub = AuthSubject::ExportedAgent(user_id, McpCredsId::new(), Vec::new());
        assert_eq!(
            actor_for_subject(&sub).unwrap(),
            ChangesetActor::User { user_id }
        );
    }

    #[test]
    fn actor_for_subject_rejects_unsupported_subjects() {
        assert!(matches!(
            actor_for_subject(&AuthSubject::Anonymous),
            Err(ChangesetError::UnsupportedActor)
        ));
    }
}
