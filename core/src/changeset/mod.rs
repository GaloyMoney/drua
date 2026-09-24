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
use crate::workflow::run::repo::WorkflowRunRepo;

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

/// Actor a given [`AuthSubject`] maps to for changeset attribution.
/// `Anonymous`/`ExportedAgent` can't open or bind a changeset yet —
/// `UnsupportedActor` — matching the handoff's actor set (§3.1): agents,
/// workflow runs, and humans via `User`.
fn actor_for_subject(sub: &AuthSubject) -> Result<ChangesetActor, ChangesetError> {
    match sub {
        AuthSubject::Agent(_, agent_id, _)
        | AuthSubject::AgentOnBehalfOfUser(_, _, agent_id, _) => Ok(ChangesetActor::Agent {
            agent_id: *agent_id,
        }),
        AuthSubject::WorkflowExecutor(_, _, run_id, _) => {
            Ok(ChangesetActor::WorkflowRun { run_id: *run_id })
        }
        AuthSubject::User(user_id) => Ok(ChangesetActor::User { user_id: *user_id }),
        AuthSubject::ExportedAgent(..) | AuthSubject::Anonymous => {
            Err(ChangesetError::UnsupportedActor)
        }
    }
}

#[derive(Clone)]
pub struct Changesets {
    repo: ChangesetRepo,
    agents: AgentRepo,
    workflow_runs: WorkflowRunRepo,
    library: drua_library::Library,
    users: crate::user::Users,
}

impl Changesets {
    pub fn new(
        pool: &sqlx::PgPool,
        agents: &AgentRepo,
        workflow_runs: &WorkflowRunRepo,
        library: &drua_library::Library,
        users: &crate::user::Users,
    ) -> Self {
        Self {
            repo: ChangesetRepo::new(pool),
            agents: agents.clone(),
            workflow_runs: workflow_runs.clone(),
            library: library.clone(),
            users: users.clone(),
        }
    }

    /// Opens a changeset at `main`'s current tip and binds `sub`'s actor
    /// to it. The branch itself (`create_ref`) is created best-effort
    /// after the DB commit — a failure here is repaired by `ensure_ref`
    /// on first use, matching the "recreate on demand" contract the
    /// design already requires for an ephemeral-clone pod restart.
    ///
    /// OQ-5 default: checked here (collection-level — "may propose at
    /// all in this project") as well as per-file at every `SpaceFs`
    /// write, so an unauthorized subject fails fast rather than after
    /// pinning `base_oid` and creating a branch.
    #[instrument(name = "domain.changeset.open", skip(self, sub))]
    pub async fn open(
        &self,
        sub: &AuthSubject,
        title: String,
        description: Option<String>,
    ) -> Result<Changeset, ChangesetError> {
        sub.can(AuthVerb::Propose, AuthResource::Space(None))?;
        let project_id = sub.project_id().ok_or(ChangesetError::NoProject)?;
        let opened_by = actor_for_subject(sub)?;
        let base_oid = self
            .library
            .fetch_and_head()
            .await?
            .ok_or(ChangesetError::MainUnborn)?;

        let mut builder = NewChangeset::builder()
            .project_id(project_id)
            .title(title)
            .base_oid(base_oid.clone())
            .opened_by(opened_by);
        if let Some(desc) = description {
            builder = builder.description(desc);
        }
        let new_changeset = builder
            .build()
            .expect("all required NewChangeset fields set");

        let mut op = self.repo.begin_op().await?;
        let changeset = self.repo.create_in_op(&mut op, new_changeset).await?;
        self.bind_actor_entity_in_op(&mut op, opened_by, changeset.id)
            .await?;
        op.commit().await?;

        if let Err(e) = self
            .library
            .create_ref(&changeset.git_ref(), &base_oid)
            .await
        {
            tracing::warn!(
                error = %e,
                changeset_id = %changeset.id,
                "changeset.open: create_ref failed; ensure_ref will repair it on first use"
            );
        }

        Audit::record_action_if_unset("changeset.open");
        Audit::record_project_id(project_id);
        Audit::record_changeset_id(changeset.id);

        Ok(changeset)
    }

    /// §2.1 resolution rule 2: the subject's bound changeset. An
    /// `Agent`/`AgentOnBehalfOfUser` with no binding of its own falls
    /// through to its workflow run's changeset (step agents inherit the
    /// run's declared changeset without their own bind).
    #[instrument(name = "domain.changeset.active_for_subject", skip(self, sub))]
    pub async fn active_for_subject(
        &self,
        sub: &AuthSubject,
    ) -> Result<Option<Changeset>, ChangesetError> {
        let id = match sub {
            AuthSubject::Agent(_, agent_id, _)
            | AuthSubject::AgentOnBehalfOfUser(_, _, agent_id, _) => {
                let agent = self.agents.find_by_id(*agent_id).await?;
                match agent.active_changeset {
                    Some(id) => Some(id),
                    None => match agent.workflow_run_id {
                        Some(run_id) => self.workflow_runs.find_by_id(run_id).await?.changeset,
                        None => None,
                    },
                }
            }
            AuthSubject::WorkflowExecutor(_, _, run_id, _) => {
                self.workflow_runs.find_by_id(*run_id).await?.changeset
            }
            AuthSubject::User(_) | AuthSubject::ExportedAgent(..) | AuthSubject::Anonymous => None,
        };
        match id {
            Some(id) => Ok(Some(self.repo.find_by_id(id).await?)),
            None => Ok(None),
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

    /// Changesets for `sub`'s own project, newest first, optionally
    /// filtered to one `status`. §8.1's `changeset list` command.
    #[instrument(name = "domain.changeset.list", skip(self, sub))]
    pub async fn list(
        &self,
        sub: &AuthSubject,
        status: Option<ChangesetStatus>,
    ) -> Result<Vec<Changeset>, ChangesetError> {
        let project_id = sub.project_id().ok_or(ChangesetError::NoProject)?;
        let mut out = Vec::new();
        let mut after = None;
        loop {
            let page = self
                .repo
                .list_for_project_id_by_created_at(
                    project_id,
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
        if let Some(status) = status {
            out.retain(|cs| cs.status == status);
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

    /// Joins an existing `Open` changeset.
    #[instrument(name = "domain.changeset.bind", skip(self, sub))]
    pub async fn bind(&self, sub: &AuthSubject, id: ChangesetId) -> Result<(), ChangesetError> {
        sub.can(AuthVerb::Propose, AuthResource::Space(None))?;
        let actor = actor_for_subject(sub)?;
        let mut op = self.repo.begin_op().await?;
        let mut cs = self.repo.find_by_id_in_op(&mut op, id).await?;
        self.check_same_project(sub, &cs)?;
        if !cs.is_open() {
            return Err(ChangesetError::InvalidTransition {
                from: cs.status,
                op: "bind",
            });
        }

        if cs.actor_bound(actor).did_execute() {
            self.repo.update_in_op(&mut op, &mut cs).await?;
        }
        self.bind_actor_entity_in_op(&mut op, actor, id).await?;
        op.commit().await?;

        Audit::record_action_if_unset("changeset.bind");
        Audit::record_changeset_id(id);
        Ok(())
    }

    /// Leaves `sub`'s bound changeset without closing it — a no-op if
    /// not bound to anything.
    #[instrument(name = "domain.changeset.unbind", skip(self, sub))]
    pub async fn unbind(&self, sub: &AuthSubject) -> Result<(), ChangesetError> {
        let actor = actor_for_subject(sub)?;
        let Some(cs) = self.active_for_subject(sub).await? else {
            return Ok(());
        };

        let mut op = self.repo.begin_op().await?;
        let mut cs = self.repo.find_by_id_in_op(&mut op, cs.id).await?;
        if cs.actor_unbound(actor).did_execute() {
            self.repo.update_in_op(&mut op, &mut cs).await?;
        }
        self.unbind_actor_entity_in_op(&mut op, actor, cs.id)
            .await?;
        op.commit().await?;

        Audit::record_action_if_unset("changeset.unbind");
        Audit::record_changeset_id(cs.id);
        Ok(())
    }

    /// `Open` bound actor, or a lead/admin closing an abandoned one;
    /// `Submitted` closes the PR (OQ-1's default) — that push/close call
    /// lands with PR 6's GitHub client, so for now the branch is just
    /// deleted locally + on origin. Unbinds every actor still pointing
    /// at this changeset.
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
        self.check_discard_authority(sub, &cs)?;

        let bound = cs.bound_actors.clone();
        if cs.discard(reason)?.did_execute() {
            self.repo.update_in_op(&mut op, &mut cs).await?;
        }
        for actor in bound {
            self.unbind_actor_entity_in_op(&mut op, actor, id).await?;
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
    /// (OQ-14's default: error, not a silent `apply`). Unbinds every
    /// actor still pointing at this changeset (OQ-8) on success.
    #[instrument(name = "domain.changeset.submit", skip(self, sub))]
    pub async fn submit(
        &self,
        sub: &AuthSubject,
        id: ChangesetId,
    ) -> Result<Changeset, ChangesetError> {
        let mut op = self.repo.begin_op().await?;
        let mut cs = self.repo.find_by_id_in_op(&mut op, id).await?;
        self.check_same_project(sub, &cs)?;
        self.check_bound_or_lead(sub, &cs, "submit")?;
        if !cs.is_open() {
            return Err(ChangesetError::InvalidTransition {
                from: cs.status,
                op: "submit",
            });
        }
        if cs.commit_count() == 0 {
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

        let bound = cs.bound_actors.clone();
        let head_oid = cs.head_oid.clone();
        if cs
            .submit(head_oid, pr.number, pr.html_url.clone())?
            .did_execute()
        {
            self.repo.update_in_op(&mut op, &mut cs).await?;
        }
        for actor in bound {
            self.unbind_actor_entity_in_op(&mut op, actor, id).await?;
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
    /// changeset — this is the collection-level twin of `open`'s
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
        self.check_bound_or_lead(sub, &cs, "apply")?;
        if !matches!(
            cs.status,
            ChangesetStatus::Open | ChangesetStatus::Submitted
        ) {
            return Err(ChangesetError::InvalidTransition {
                from: cs.status,
                op: "apply",
            });
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

        let bound = cs.bound_actors.clone();
        let pr_number = cs.pr_number;
        if cs.apply(merge_oid.clone(), actor)?.did_execute() {
            self.repo.update_in_op(&mut op, &mut cs).await?;
        }
        for a in bound {
            self.unbind_actor_entity_in_op(&mut op, a, id).await?;
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
        self.check_bound_or_lead(sub, &cs, "rebase")?;
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
        if let Some(base) = self.library.merge_base(main_oid, &cs.head_oid).await? {
            if base == cs.head_oid {
                self.mark_merged_in_op(cs.id, main_oid).await?;
                return Ok(());
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
        let bound = cs.bound_actors.clone();
        if cs.mark_merged(merge_oid.to_string())?.did_execute() {
            self.repo.update_in_op(&mut op, &mut cs).await?;
        }
        for actor in bound {
            self.unbind_actor_entity_in_op(&mut op, actor, id).await?;
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
        let bound = cs.bound_actors.clone();
        if cs.mark_abandoned()?.did_execute() {
            self.repo.update_in_op(&mut op, &mut cs).await?;
        }
        for actor in bound {
            self.unbind_actor_entity_in_op(&mut op, actor, id).await?;
        }
        op.commit().await?;

        Audit::record_action_if_unset("changeset.observe_abandoned");
        Audit::record_changeset_id(id);
        Ok(())
    }

    /// Same-project check backing every read/write access to an
    /// already-resolved changeset (`status`, `bind`, `discard`,
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
        match sub.project_id() {
            Some(pid) if pid == cs.project_id => Ok(()),
            Some(_) => Err(ChangesetError::Foreign { id: cs.id }),
            None => Ok(()),
        }
    }

    /// §7/OQ-1's `discard` rule: an `Open` changeset's own bound actor
    /// may discard it; a `Submitted` one (closing its PR) — or an
    /// abandoned `Open` one nobody is bound to anymore — needs a lead
    /// or admin. `has_scope` treats `User` subjects as having every
    /// scope, so this also covers "Users are omnipotent" without a
    /// separate branch.
    fn check_discard_authority(
        &self,
        sub: &AuthSubject,
        cs: &Changeset,
    ) -> Result<(), ChangesetError> {
        if sub.is_admin() || sub.has_scope(&AuthScope::ProjectAdmin(cs.project_id)) {
            return Ok(());
        }
        if cs.status == ChangesetStatus::Open {
            if let Ok(actor) = actor_for_subject(sub) {
                if cs.bound_actors.contains(&actor) {
                    return Ok(());
                }
            }
        }
        Err(ChangesetError::Forbidden {
            id: cs.id,
            action: "discard",
        })
    }

    /// OQ-7 default: only `cs`'s bound actor, or a lead/admin, may
    /// `submit`/`apply`/`rebase` it — status-independent, unlike
    /// `discard`'s OQ-1 rule (`check_discard_authority`), since none
    /// of these three are meaningful on an already-closed changeset
    /// anyway (the caller's own status check catches that separately).
    fn check_bound_or_lead(
        &self,
        sub: &AuthSubject,
        cs: &Changeset,
        action: &'static str,
    ) -> Result<(), ChangesetError> {
        if sub.is_admin() || sub.has_scope(&AuthScope::ProjectAdmin(cs.project_id)) {
            return Ok(());
        }
        if let Ok(actor) = actor_for_subject(sub) {
            if cs.bound_actors.contains(&actor) {
                return Ok(());
            }
        }
        Err(ChangesetError::Forbidden { id: cs.id, action })
    }

    async fn bind_actor_entity_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        actor: ChangesetActor,
        changeset_id: ChangesetId,
    ) -> Result<(), ChangesetError> {
        match actor {
            ChangesetActor::Agent { agent_id } => {
                let mut agent = self.agents.find_by_id_in_op(&mut *op, agent_id).await?;
                if agent.changeset_bound(changeset_id)?.did_execute() {
                    self.agents.update_in_op(op, &mut agent).await?;
                }
            }
            ChangesetActor::WorkflowRun { run_id } => {
                let mut run = self
                    .workflow_runs
                    .find_by_id_in_op(&mut *op, run_id)
                    .await?;
                if run.changeset_opened(changeset_id).did_execute() {
                    self.workflow_runs.update_in_op(op, &mut run).await?;
                }
            }
            ChangesetActor::User { .. } => {}
        }
        Ok(())
    }

    async fn unbind_actor_entity_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        actor: ChangesetActor,
        changeset_id: ChangesetId,
    ) -> Result<(), ChangesetError> {
        match actor {
            ChangesetActor::Agent { agent_id } => {
                let mut agent = self.agents.find_by_id_in_op(&mut *op, agent_id).await?;
                if agent.changeset_unbound(changeset_id).did_execute() {
                    self.agents.update_in_op(op, &mut agent).await?;
                }
            }
            ChangesetActor::WorkflowRun { run_id } => {
                let mut run = self
                    .workflow_runs
                    .find_by_id_in_op(&mut *op, run_id)
                    .await?;
                if run.changeset_closed(changeset_id).did_execute() {
                    self.workflow_runs.update_in_op(op, &mut run).await?;
                }
            }
            ChangesetActor::User { .. } => {}
        }
        Ok(())
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
            .project_id(ProjectId::new())
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
            .project_id(ProjectId::new())
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

    #[test]
    fn actor_for_subject_rejects_unsupported_subjects() {
        assert!(matches!(
            actor_for_subject(&AuthSubject::Anonymous),
            Err(ChangesetError::UnsupportedActor)
        ));
    }
}
