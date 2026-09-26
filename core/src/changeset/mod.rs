pub mod entity;
pub mod error;
pub mod repo;

use tracing::instrument;

pub use entity::*;
pub use error::ChangesetError;
use repo::ChangesetRepo;

use crate::agent::repo::AgentRepo;
use crate::audit::Audit;
use crate::auth::{AuthResource, AuthScope, AuthSubject, AuthVerb};
use crate::primitives::*;

/// One touched path in a [`ChangesetStatusView`], repo-relative to the
/// space it belongs to (not the repo root).
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
    pub touched: Vec<TouchedFile>,
    pub pr_url: Option<String>,
}

/// Actor a given [`AuthSubject`] maps to for changeset attribution —
/// also the `draft_for`/`find_open_for_actor` lookup key (rev2 D4).
/// `Anonymous` still can't act on a changeset (`UnsupportedActor`);
/// `ExportedAgent` is a first-class actor as of rev2 D11 — it maps to
/// `User` (the synthetic user id for agent-owned MCP creds is fine as
/// a draft key: it's stable and unique per credential-owning agent).
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

#[derive(Clone)]
pub struct Changesets {
    repo: ChangesetRepo,
    agents: AgentRepo,
    library: drua_library::Library,
    users: crate::user::Users,
}

impl Changesets {
    /// `WorkflowRunRepo` isn't wired here (rev2 D4: no lookup goes
    /// through `Agent`/`WorkflowRun` any more — `draft_for` reads its
    /// own `opened_by_actor` index). The executor/`Workflows` service
    /// still own their run's `changeset_opened`/`changeset_closed`
    /// history directly against their own `WorkflowRunRepo`.
    pub fn new(
        pool: &sqlx::PgPool,
        agents: &AgentRepo,
        library: &drua_library::Library,
        users: &crate::user::Users,
    ) -> Self {
        Self {
            repo: ChangesetRepo::new(pool),
            agents: agents.clone(),
            library: library.clone(),
            users: users.clone(),
        }
    }

    /// rev2 D8's step-agent rule (rev1 §2.1 rule 2 survives): a step
    /// agent's draft key is its run, not itself — so two step agents in
    /// the same run share one draft. Resolved by fetching the `Agent`
    /// and checking `workflow_run_id` first; every other subject maps
    /// via `actor_for_subject` directly.
    async fn actor_key_for(&self, sub: &AuthSubject) -> Result<ChangesetActor, ChangesetError> {
        if let AuthSubject::Agent(_, agent_id, _)
        | AuthSubject::AgentOnBehalfOfUser(_, _, agent_id, _) = sub
        {
            // Best-effort: an `Agent` lookup miss (a synthetic/test
            // subject, or a genuinely deleted agent racing a write in
            // flight) falls back to the agent's own identity rather
            // than hard-failing the write — this lookup exists only to
            // *narrow* the key for a step agent, never to gate whether
            // one can act at all.
            match self.agents.find_by_id(*agent_id).await {
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

    /// rev2 D4/§3 rule 3: `sub`'s current `Open` draft, if any — a pure
    /// read, one indexed query (`opened_by_actor`), never creates one.
    /// Used by a read-intent `space:`/`draft:` resolution (nothing to
    /// overlay if no draft exists yet) and the `spaces draft` command.
    #[instrument(name = "domain.changeset.open_draft_for", skip(self, sub))]
    pub async fn open_draft_for(
        &self,
        sub: &AuthSubject,
    ) -> Result<Option<Changeset>, ChangesetError> {
        let actor = self.actor_key_for(sub).await?;
        self.find_open_for_actor(actor).await
    }

    /// The run's current `Open` draft, if any — the same lookup as
    /// `open_draft_for`, but keyed directly by `run_id` rather than a
    /// resolved `AuthSubject`. The executor's run-end close and
    /// `Workflows::cancel_run` call this with only a `WorkflowRunId`
    /// in hand (a `WorkflowExecutor` subject only exists mid-run).
    pub async fn open_draft_for_run(
        &self,
        run_id: WorkflowRunId,
    ) -> Result<Option<Changeset>, ChangesetError> {
        self.find_open_for_actor(ChangesetActor::WorkflowRun { run_id })
            .await
    }

    /// The most recent changeset opened by `actor`, if it's still
    /// `Open`. `list_for_opened_by_actor_by_created_at` orders by
    /// `created_at` descending — since the partial unique index allows
    /// at most one `Open` row per actor at a time, and a fresh draft is
    /// always younger than whatever terminal changeset preceded it for
    /// the same actor, "most recent for this actor" is exactly "the
    /// open one, if any" without a second query or a status filter.
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

    /// rev2 D2/D4: `sub`'s draft, created lazily on first use. No
    /// `open` verb — this *is* the only way a changeset comes into
    /// existence outside the executor's declared-title pre-create.
    ///
    /// `title`/`description` are used only when actually creating (a
    /// `None` title is derived from the actor + `first_touched_path`,
    /// §3: "Draft by \<agent name | user email\> — \<first touched
    /// path\>"). The branch itself (`create_ref`) is created
    /// best-effort after the DB commit, matching `apply`'s "recreate
    /// on demand" contract for an ephemeral-clone pod restart.
    ///
    /// Concurrent first-writes from the same actor race on the
    /// `changesets_opened_by_actor_key` partial unique index rather
    /// than a lock: the loser's `create_in_op` fails `was_duplicate`,
    /// and it re-reads the winner's row instead of erroring.
    #[instrument(name = "domain.changeset.draft_for", skip(self, sub, description))]
    pub async fn draft_for(
        &self,
        sub: &AuthSubject,
        title: Option<String>,
        description: Option<String>,
        first_touched_path: Option<&str>,
    ) -> Result<Changeset, ChangesetError> {
        sub.can(AuthVerb::Propose, AuthResource::Space(None))?;
        let actor = self.actor_key_for(sub).await?;
        if let Some(cs) = self.find_open_for_actor(actor).await? {
            return Ok(cs);
        }

        // rev3 D16: `None` for a project-less subject (a bare `Admin`)
        // — the draft is actor-owned, so it has no project requirement
        // of its own.
        let project_id = sub.effective_project_id();
        let base_oid = self
            .library
            .fetch_and_head()
            .await?
            .ok_or(ChangesetError::MainUnborn)?;
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

        if let Err(e) = self
            .library
            .create_ref(&changeset.git_ref(), &base_oid)
            .await
        {
            tracing::warn!(
                error = %e,
                changeset_id = %changeset.id,
                "changeset.draft_for: create_ref failed; ensure_ref will repair it on first use"
            );
        }

        Audit::record_action_if_unset("changeset.draft_for");
        if let Some(project_id) = project_id {
            Audit::record_project_id(project_id);
        }
        Audit::record_changeset_id(changeset.id);

        Ok(changeset)
    }

    /// §3's derived title: the acting agent's name, or the acting
    /// user's email, falling back to `describe_actor`'s generic form
    /// when the lookup fails (e.g. a since-deleted agent) — a draft
    /// still needs a title, just a less friendly one.
    async fn derive_draft_title(
        &self,
        actor: ChangesetActor,
        first_touched_path: Option<&str>,
    ) -> String {
        let who = match actor {
            ChangesetActor::Agent { agent_id } => match self.agents.find_by_id(agent_id).await {
                Ok(agent) => agent.name,
                Err(_) => describe_actor(&actor),
            },
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

    /// Resolves `id` for `SpaceFs`'s target resolution (both the
    /// explicit `@id` override and a stale-bind safety check): applies
    /// the same-project rule (`check_same_project`) but — unlike
    /// `status`/`bind`/`discard` — does not require `Open`. Reads of a
    /// `Submitted` (or even landed) changeset via `@id` stay legible to
    /// reviewers; `SpaceFs` itself rejects a *write* to a non-`Open`
    /// target with `SpaceError::ChangesetNotOpen`.
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

    /// Repairs a missing local ref (ephemeral-clone pod restart, or a
    /// prune race on a never-pushed branch — see `fetch_origin`'s prune
    /// comment) by recreating it from the entity's `head_oid`. Internal:
    /// callers resolve a changeset's tip through this, never by reading
    /// `resolve_ref` directly.
    pub(crate) async fn ensure_ref(&self, cs: &Changeset) -> Result<String, ChangesetError> {
        if let Some(tip) = self.library.resolve_ref(&cs.git_ref()).await? {
            return Ok(tip);
        }
        self.library.create_ref(&cs.git_ref(), &cs.head_oid).await?;
        Ok(cs.head_oid.clone())
    }

    /// Called by `SpaceFs` after a successful write to a changeset
    /// target. No auth — internal plumbing, not a subject-facing verb.
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

    /// Changesets visible to `sub`, newest first, optionally filtered
    /// to one `status` (§8.1's `changeset list` command; rev3 §4/§6.1
    /// `list-drafts`). rev3 D4: a project-scoped subject sees its
    /// project's changesets union its own (covers the edge case of an
    /// own draft that isn't project-scoped); an `Admin` sees every
    /// changeset, optionally narrowed to one `project_id` (rev3 OQ-21
    /// — admin, no filter, means everything).
    #[instrument(name = "domain.changeset.list", skip(self, sub))]
    pub async fn list(
        &self,
        sub: &AuthSubject,
        status: Option<ChangesetStatus>,
        project_id: Option<ProjectId>,
    ) -> Result<Vec<Changeset>, ChangesetError> {
        let mut out = if sub.is_admin() {
            match project_id {
                Some(pid) => self.list_all_for_project(pid).await?,
                None => self.list_all_unfiltered().await?,
            }
        } else {
            let mut acc = match sub.effective_project_id() {
                Some(pid) => self.list_all_for_project(pid).await?,
                None => Vec::new(),
            };
            if let Ok(actor) = self.actor_key_for(sub).await {
                for cs in self.list_all_for_actor(actor).await? {
                    if !acc.iter().any(|x| x.id == cs.id) {
                        acc.push(cs);
                    }
                }
            }
            acc
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

    async fn list_all_for_actor(
        &self,
        actor: ChangesetActor,
    ) -> Result<Vec<Changeset>, ChangesetError> {
        let mut out = Vec::new();
        let mut after = None;
        loop {
            let page = self
                .repo
                .list_for_opened_by_actor_by_created_at(
                    actor.to_string(),
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

    /// Read gate: any subject in the same project (or without a project
    /// context — see the caveat on `check_same_project`) may read
    /// status; write verbs aren't involved.
    #[instrument(name = "domain.changeset.status", skip(self, sub))]
    pub async fn status(
        &self,
        sub: &AuthSubject,
        id: ChangesetId,
    ) -> Result<ChangesetStatusView, ChangesetError> {
        let cs = self.repo.find_by_id(id).await?;
        self.check_same_project(sub, &cs)?;

        let main_oid = self
            .library
            .resolve_ref("refs/heads/main")
            .await?
            .ok_or(ChangesetError::MainUnborn)?;
        let (mergeable, conflicts) = match self
            .library
            .merge_trees(&cs.base_oid, &cs.head_oid, &main_oid)
            .await?
        {
            Ok(_) => (true, Vec::new()),
            Err(paths) => (false, paths),
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

    /// `Open` owner, or a lead/admin closing an abandoned one;
    /// `Submitted` closes the PR (OQ-1's default) — that push/close call
    /// lands with PR 6's GitHub client, so for now the branch is just
    /// deleted locally + on origin.
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

        if let Err(e) = self.library.delete_ref(&cs.git_ref(), true).await {
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

    /// Opens a PR `cs`'s branch → `main` (§7, §10). `Open` only, and
    /// only when there's something to review (`Empty` on zero
    /// commits) and it would merge cleanly (`Conflicts` — checked up
    /// front so a doomed PR is never opened). `PrUnavailable` when the
    /// library has no GitHub App / isn't a `github.com` remote
    /// (OQ-14's default: error, not a silent `apply`).
    #[instrument(name = "domain.changeset.submit", skip(self, sub))]
    pub async fn submit(
        &self,
        sub: &AuthSubject,
        id: ChangesetId,
    ) -> Result<Changeset, ChangesetError> {
        let mut op = self.repo.begin_op().await?;
        let mut cs = self.repo.find_by_id_in_op(&mut op, id).await?;
        self.check_same_project(sub, &cs)?;
        self.check_owner_or_lead(sub, &cs, "submit").await?;
        if !cs.is_open() {
            return Err(ChangesetError::InvalidTransition {
                from: cs.status,
                op: "submit",
            });
        }
        if !cs.has_commits() {
            return Err(ChangesetError::Empty { id });
        }

        let main_oid = self
            .library
            .resolve_ref("refs/heads/main")
            .await?
            .ok_or(ChangesetError::MainUnborn)?;
        if let Err(paths) = self
            .library
            .merge_trees(&cs.base_oid, &cs.head_oid, &main_oid)
            .await?
        {
            return Err(ChangesetError::Conflicts { id, paths });
        }

        let (owner, repo) = self
            .library
            .repo_coord()
            .ok_or(ChangesetError::PrUnavailable)?;
        let github = self
            .library
            .github_app()
            .ok_or(ChangesetError::PrUnavailable)?;
        let touched = self.touched_files(&cs).await?;
        let body = render_pr_body(&cs, &touched);
        let pr = github
            .create_pull(&owner, &repo, &cs.branch(), "main", &cs.title, &body)
            .await?;

        let head_oid = cs.head_oid.clone();
        if cs
            .submit(head_oid, pr.number, pr.html_url.clone())?
            .did_execute()
        {
            self.repo.update_in_op(&mut op, &mut cs).await?;
        }
        op.commit().await?;

        Audit::record_action_if_unset("changeset.submit");
        Audit::record_changeset_id(id);
        Ok(cs)
    }

    /// Merges `cs`'s tip into `main` directly (§7). `Open` or
    /// `Submitted`; requires the subject to be able to `Update`
    /// `main` at all (per-space `Update` was already required for
    /// every individual write that landed on `main` bypassing a
    /// changeset — this is the collection-level twin of `draft_for`'s
    /// `Propose`-on-`Space(None)` check, for the same "fail before
    /// doing any work" reason). Closes the PR (best-effort) if one was
    /// open, and deletes the branch — a merge commit without the
    /// `Drua-Projection` trailer, so the reverse-sync importer picks
    /// it up like any human-authored commit.
    /// Returns the entity alongside the merge commit's oid — `merge_oid`
    /// only ever lives in the `Applied` event (§3.1's `ChangesetEvent`),
    /// never projected onto a builder field, so the caller (the
    /// `changeset` tool) needs it handed back explicitly rather than
    /// re-deriving it from history.
    #[instrument(name = "domain.changeset.apply", skip(self, sub))]
    pub async fn apply(
        &self,
        sub: &AuthSubject,
        id: ChangesetId,
    ) -> Result<(Changeset, String), ChangesetError> {
        sub.can(AuthVerb::Update, AuthResource::Space(None))?;
        let mut op = self.repo.begin_op().await?;
        let mut cs = self.repo.find_by_id_in_op(&mut op, id).await?;
        self.check_same_project(sub, &cs)?;
        self.check_owner_or_lead(sub, &cs, "apply").await?;
        if !matches!(
            cs.status,
            ChangesetStatus::Open | ChangesetStatus::Submitted
        ) {
            return Err(ChangesetError::InvalidTransition {
                from: cs.status,
                op: "apply",
            });
        }
        // `submit`'s own `Empty` check doesn't cover `apply`: a lead can
        // land any `Open` draft directly, and a YAML `changeset:` block
        // pre-creates the run's draft before any step writes to it — so
        // without this, an unused draft's `publish` would land a no-op
        // merge commit on `main` (bugbot 2026-09-25). Uses `has_commits`,
        // not `commit_count`, so a cleanly rebased draft with real prior
        // content isn't rejected as empty (bugbot 2026-09-26).
        if !cs.has_commits() {
            return Err(ChangesetError::Empty { id });
        }

        let main_oid = self
            .library
            .resolve_ref("refs/heads/main")
            .await?
            .ok_or(ChangesetError::MainUnborn)?;
        if let Err(paths) = self
            .library
            .merge_trees(&cs.base_oid, &cs.head_oid, &main_oid)
            .await?
        {
            return Err(ChangesetError::Conflicts { id, paths });
        }

        let actor = actor_for_subject(sub)?;
        let attribution = self.users.commit_attribution().await;
        let message = format!(
            "changeset: {}\n\n{}",
            cs.title,
            cs.description.clone().unwrap_or_default(),
        );
        let merge_oid = self
            .library
            .merge_into_main(&cs.head_oid, message, attribution)
            .await?;

        let pr_number = cs.pr_number;
        if cs.apply(merge_oid.clone(), actor)?.did_execute() {
            self.repo.update_in_op(&mut op, &mut cs).await?;
        }
        op.commit().await?;

        if let (Some(pr_number), Some(github), Some((owner, repo))) = (
            pr_number,
            self.library.github_app(),
            self.library.repo_coord(),
        ) {
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
        if let Err(e) = self.library.delete_ref(&cs.git_ref(), true).await {
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

    /// Moves `cs` onto current `main`. `Open` only. Squash (§7/OQ-9,
    /// deliberate — per-op branch history has no value after a
    /// rebase; the PR body still lists every op via `touched_files`,
    /// computed from base..head regardless of commit count).
    /// Conflicts leave the ref untouched, so a failed rebase is safe
    /// to retry after the agent resolves them by hand.
    #[instrument(name = "domain.changeset.rebase", skip(self, sub))]
    pub async fn rebase(
        &self,
        sub: &AuthSubject,
        id: ChangesetId,
    ) -> Result<Changeset, ChangesetError> {
        let mut op = self.repo.begin_op().await?;
        let mut cs = self.repo.find_by_id_in_op(&mut op, id).await?;
        self.check_same_project(sub, &cs)?;
        self.check_owner_or_lead(sub, &cs, "rebase").await?;
        if !cs.is_open() {
            return Err(ChangesetError::InvalidTransition {
                from: cs.status,
                op: "rebase",
            });
        }

        let main_oid = self
            .library
            .resolve_ref("refs/heads/main")
            .await?
            .ok_or(ChangesetError::MainUnborn)?;
        let attribution = self.users.commit_attribution().await;
        let message = format!("changeset: {} (rebased)", cs.title);
        match self
            .library
            .rebase_ref(&cs.git_ref(), &main_oid, message, attribution)
            .await?
        {
            Ok((new_base, new_head)) => {
                if cs.rebase(new_base, new_head)?.did_execute() {
                    self.repo.update_in_op(&mut op, &mut cs).await?;
                }
                op.commit().await?;
                Audit::record_action_if_unset("changeset.rebase");
                Audit::record_changeset_id(id);
                Ok(cs)
            }
            Err(paths) => Err(ChangesetError::Conflicts { id, paths }),
        }
    }

    /// Called by `Library::on_head_advanced` after each processed sync
    /// tick (§11). No auth — internal plumbing driven by the git fetch
    /// loop, never a subject action. Sweeps every `Open`/`Submitted`
    /// changeset across every project (sync isn't project-scoped) for:
    /// merged (`head_oid` now an ancestor of `main`) → `Merged`;
    /// `Submitted` with its ref gone on origin (only possible once
    /// `fetch_origin`'s prune is on — PR 1) and not merged →
    /// `Abandoned`; ref present but its tip has moved past `head_oid`
    /// (a human pushed directly) → recorded as an `external` commit
    /// (OQ-4's default). Per-changeset failures are logged and
    /// swallowed — one bad ref must not block observing the rest.
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
        // A changeset that hasn't diverged from its own base yet
        // (`head_oid == base_oid` — no commits recorded) trivially
        // satisfies "head is an ancestor of main" the moment `base_oid`
        // was pinned to main's tip, well before it's actually merged.
        // rev2 surfaced this: lazy drafts spend real time in exactly
        // this state (created on first write, momentarily empty) far
        // more often than rev1's explicit `open`+immediate-bind did, so
        // a sync tick landing in that window would wrongly mark a
        // brand-new draft `Merged` and delete its ref out from under
        // the write that's about to land on it.
        if cs.head_oid != cs.base_oid {
            if let Some(base) = self.library.merge_base(main_oid, &cs.head_oid).await? {
                if base == cs.head_oid {
                    self.mark_merged_in_op(cs.id, main_oid).await?;
                    return Ok(());
                }
            }
        }

        let ref_oid = self.library.resolve_ref(&cs.git_ref()).await?;
        if status == ChangesetStatus::Submitted && ref_oid.is_none() {
            self.mark_abandoned_in_op(cs.id).await?;
            return Ok(());
        }

        if let Some(tip) = ref_oid {
            if tip != cs.head_oid {
                self.record_commit(cs.id, tip, "external", "").await?;
            }
        }
        Ok(())
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

        if let Err(e) = self.library.delete_ref(&cs.git_ref(), true).await {
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

    /// Same-project check backing every read/write access to an
    /// already-resolved changeset (`status`, `discard`,
    /// `find_for_target`). Deliberately membership-based rather than a
    /// `sub.can(Read, Space(None))` scope check — the latter would deny
    /// a plain `ProjectMember` its own project's changesets, since
    /// `Space(None)` grants stay "as today" (§4.2) for that scope.
    ///
    /// A subject with no project context (`User`, `ExportedAgent`,
    /// `Anonymous`) is waved through rather than rejected here — safe
    /// because every real caller reaches this only after
    /// `Projects::space_for_subject`'s mount gate, which already
    /// requires `sub.project_id().is_some()` for anyone but an admin
    /// (`User`s are the one case that's genuinely omnipotent).
    fn check_same_project(&self, sub: &AuthSubject, cs: &Changeset) -> Result<(), ChangesetError> {
        match (sub.effective_project_id(), cs.project_id) {
            (Some(pid), Some(cpid)) if pid == cpid => Ok(()),
            (Some(_), Some(_)) => Err(ChangesetError::Foreign { id: cs.id }),
            // A project-less changeset (rev3 D16: a bare-`Admin`'s
            // draft) is never "foreign" to a project-scoped subject in
            // this read-gate sense — ownership, not project, is what
            // actually restricts it (`check_owner_or_lead`/
            // `check_discard_authority`).
            (Some(_), None) => Ok(()),
            (None, _) => Ok(()),
        }
    }

    /// §7/OQ-1's `discard` rule: an `Open` changeset's own owner may
    /// discard it; a `Submitted` one (closing its PR) — or an
    /// abandoned `Open` one its owner walked away from — needs a lead
    /// or admin. `has_scope` treats `User` subjects as having every
    /// scope, so this also covers "Users are omnipotent" without a
    /// separate branch.
    async fn check_discard_authority(
        &self,
        sub: &AuthSubject,
        cs: &Changeset,
    ) -> Result<(), ChangesetError> {
        if sub.is_admin()
            || cs
                .project_id
                .is_some_and(|pid| sub.has_scope(&AuthScope::ProjectAdmin(pid)))
        {
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

    /// rev2 OQ-7 default: only `cs`'s owner, or a lead/admin, may
    /// `submit`/`apply`/`rebase` it — status-independent, unlike
    /// `discard`'s OQ-1 rule (`check_discard_authority`), since none
    /// of these three are meaningful on an already-closed changeset
    /// anyway (the caller's own status check catches that separately).
    /// Renamed from rev1's `check_bound_or_lead` — there is no bind
    /// state left to check, only ownership (D4).
    ///
    /// Resolves the owner check through `actor_key_for`, the same
    /// lookup `draft_for` uses to key the draft — not the cheaper
    /// `actor_for_subject`. A workflow step agent's draft is keyed by
    /// `WorkflowRun` (rev2 D8: two step agents in the same run share
    /// one draft), so `actor_for_subject`'s plain `Agent` mapping never
    /// matches `cs.opened_by` and the very agent that created the draft
    /// could never `submit`/`apply`/`rebase` it (bugbot 2026-09-25).
    async fn check_owner_or_lead(
        &self,
        sub: &AuthSubject,
        cs: &Changeset,
        action: &'static str,
    ) -> Result<(), ChangesetError> {
        if sub.is_admin()
            || cs
                .project_id
                .is_some_and(|pid| sub.has_scope(&AuthScope::ProjectAdmin(pid)))
        {
            return Ok(());
        }
        if let Ok(actor) = self.actor_key_for(sub).await {
            if cs.opened_by == actor {
                return Ok(());
            }
        }
        Err(ChangesetError::Forbidden { id: cs.id, action })
    }

    /// The touched-file count at `cs`'s current tip vs its base —
    /// D10's stamp `<n> files`. A thin, cheap wrapper around
    /// `touched_files` (one git diff, no merge simulation) so
    /// `SpaceFs` doesn't have to run `status`'s full mergeability
    /// check just to render a stamp.
    pub(crate) async fn touched_count(&self, cs: &Changeset) -> Result<usize, ChangesetError> {
        Ok(self.touched_files(cs).await?.len())
    }

    /// D19: `cs`'s touched paths, scoped to one space — used only by
    /// `SpaceFs`'s `space:` read "differs in your draft" stamp.
    pub(crate) async fn touched_paths_in(
        &self,
        cs: &Changeset,
        slug: &str,
    ) -> Result<Vec<String>, ChangesetError> {
        Ok(self
            .touched_files(cs)
            .await?
            .into_iter()
            .filter(|t| t.space_slug == slug)
            .map(|t| t.path)
            .collect())
    }

    async fn touched_files(&self, cs: &Changeset) -> Result<Vec<TouchedFile>, ChangesetError> {
        let deltas = self
            .library
            .changes_since(Some(&cs.base_oid), &cs.head_oid)
            .await?;
        let mut out = Vec::with_capacity(deltas.len());
        for delta in deltas {
            let Some((space_slug, path)) = split_space_path(&delta.path) else {
                continue;
            };
            out.push(TouchedFile {
                space_slug,
                path,
                kind: delta.kind.into(),
            });
        }
        Ok(out)
    }
}

/// `spaces/<slug>/<rel>` → `(slug, rel)`. `None` for anything outside
/// `spaces/` (a changeset should never touch anything else, but
/// `status` shouldn't panic if it somehow does).
fn split_space_path(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix("spaces/")?;
    let (slug, rel) = rest.split_once('/')?;
    Some((slug.to_string(), rel.to_string()))
}

fn describe_actor(actor: &ChangesetActor) -> String {
    match actor {
        ChangesetActor::Agent { agent_id } => format!("agent {agent_id}"),
        ChangesetActor::WorkflowRun { run_id } => format!("workflow run {run_id}"),
        ChangesetActor::User { user_id } => format!("user {user_id}"),
    }
}

/// §10's PR body template — plain markdown, deterministic. Repeating
/// the trailers here (branch commits already carry them via
/// `user::commit_attribution`'s trailer loop) is what preserves
/// provenance through a squash-merge on GitHub's side.
fn render_pr_body(cs: &Changeset, touched: &[TouchedFile]) -> String {
    let mut body = match cs.description.as_deref() {
        Some(d) if !d.is_empty() => d.to_string(),
        _ => "(no description)".to_string(),
    };
    body.push_str("\n\n");
    let base_short = &cs.base_oid[..cs.base_oid.len().min(8)];
    body.push_str(&format!(
        "**Changeset** `{}` · opened by {} · base `{base_short}` · {} commit(s)\n",
        cs.id,
        describe_actor(&cs.opened_by),
        cs.commit_count(),
    ));

    if !touched.is_empty() {
        body.push_str("\n| op | path |\n|---|---|\n");
        for t in touched {
            let op = match t.kind {
                TouchedKind::Added => "add",
                TouchedKind::Modified => "edit",
                TouchedKind::Deleted => "delete",
            };
            body.push_str(&format!("| {op} | space:{}/{} |\n", t.space_slug, t.path));
        }
    }

    body.push_str(&format!("\nDrua-Changeset: {}\n", cs.id));
    if let Some(run_id) = cs.opened_by.workflow_run_id() {
        body.push_str(&format!("Drua-Workflow-Run: {run_id}\n"));
    }
    if let ChangesetActor::User { user_id } = &cs.opened_by {
        body.push_str(&format!("Drua-Acting-User: {user_id}\n"));
    }
    body
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
    fn pr_body_includes_description_id_and_base() {
        let cs = open_changeset();
        let body = render_pr_body(&cs, &[]);
        assert!(body.starts_with("moves stale notes into the new effort"));
        assert!(body.contains(&format!("**Changeset** `{}`", cs.id)));
        assert!(body.contains("base `aaaaaaaa`"));
        assert!(body.contains("0 commit(s)"));
        assert!(body.contains(&format!("Drua-Changeset: {}", cs.id)));
    }

    #[test]
    fn pr_body_falls_back_when_no_description() {
        let new = NewChangeset::builder()
            .project_id(Some(ProjectId::new()))
            .title("t")
            .base_oid("a")
            .opened_by(ChangesetActor::Agent {
                agent_id: AgentId::new(),
            })
            .build()
            .unwrap();
        let cs = Changeset::try_from_events(new.into_events()).unwrap();
        let body = render_pr_body(&cs, &[]);
        assert!(body.starts_with("(no description)"));
    }

    #[test]
    fn pr_body_lists_touched_files_as_a_table() {
        let cs = open_changeset();
        let touched = vec![
            TouchedFile {
                space_slug: "docs".to_string(),
                path: "a.md".to_string(),
                kind: TouchedKind::Modified,
            },
            TouchedFile {
                space_slug: "docs".to_string(),
                path: "b.md".to_string(),
                kind: TouchedKind::Added,
            },
        ];
        let body = render_pr_body(&cs, &touched);
        assert!(body.contains("| edit | space:docs/a.md |"));
        assert!(body.contains("| add | space:docs/b.md |"));
    }

    #[test]
    fn split_space_path_extracts_slug_and_rel() {
        assert_eq!(
            split_space_path("spaces/drua-dev/efforts/x/a.md"),
            Some(("drua-dev".to_string(), "efforts/x/a.md".to_string()))
        );
    }

    #[test]
    fn split_space_path_rejects_non_space_paths() {
        assert_eq!(split_space_path("other/thing.md"), None);
        assert_eq!(split_space_path("spaces/only-slug"), None);
        assert_eq!(split_space_path(""), None);
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

    /// rev2 D11: an external MCP agent is a first-class actor now,
    /// mapped to `User` via its (possibly synthetic) user id — not
    /// `UnsupportedActor` as in rev1.
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
