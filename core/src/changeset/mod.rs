pub mod entity;
pub mod error;
pub mod repo;

use tracing::instrument;

pub use entity::*;
pub use error::ChangesetError;
use repo::ChangesetRepo;

use crate::agent::repo::AgentRepo;
use crate::audit::Audit;
use crate::auth::error::AuthorizationError;
use crate::auth::{AuthResource, AuthSubject, AuthVerb};
use crate::primitives::*;

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

    async fn actor_key_for(&self, sub: &AuthSubject) -> Result<ChangesetActor, ChangesetError> {
        if let AuthSubject::Agent(_, agent_id, _)
        | AuthSubject::AgentOnBehalfOfUser(_, _, agent_id, _) = sub
        {
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

    pub(crate) async fn ensure_ref(&self, cs: &Changeset) -> Result<String, ChangesetError> {
        if let Some(tip) = self.library.resolve_ref(&cs.git_ref()).await? {
            return Ok(tip);
        }
        self.library.create_ref(&cs.git_ref(), &cs.head_oid).await?;
        Ok(cs.head_oid.clone())
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
            "{}\n\n{}",
            title
                .clone()
                .unwrap_or_else(|| format!("changeset: {}", cs.title)),
            body.clone()
                .or_else(|| cs.description.clone())
                .unwrap_or_default(),
        );
        let merge_oid = self
            .library
            .merge_into_main(&cs.head_oid, message, attribution)
            .await?;

        let pr_number = cs.pr_number;
        if cs
            .apply(merge_oid.clone(), actor, title, body)?
            .did_execute()
        {
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

        let main_oid = self
            .library
            .resolve_ref("refs/heads/main")
            .await?
            .ok_or(ChangesetError::MainUnborn)?;
        let attribution = self.users.commit_attribution().await;
        let message = format!("changeset: {} (rebased)", cs.title);
        match self
            .library
            .rebase_ref(&cs.git_ref(), &main_oid, &cs.head_oid, message, attribution)
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
