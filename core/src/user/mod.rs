mod entity;
pub mod error;
pub(crate) mod repo;

use drua_library::{CommitAttribution, CommitSubjectKind};
use tracing::instrument;

pub use entity::User;
use entity::*;
pub use error::*;
use repo::*;

use crate::audit::{Audit, AuditContextData};
use crate::primitives::*;

#[derive(Clone)]
pub struct Users {
    repo: UserRepo,
}

impl Users {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        let repo = UserRepo::new(pool);
        Self { repo }
    }

    #[instrument(name = "domain.user.find_by_github_id", skip(self))]
    pub async fn find_by_github_id(&self, github_id: &str) -> Result<Option<User>, UserError> {
        Ok(self.repo.maybe_find_by_github_id(github_id).await?)
    }

    #[instrument(name = "domain.user.create_from_github_login", skip(self))]
    pub async fn create_from_github_login(
        &self,
        github_id: impl Into<String> + std::fmt::Debug,
        email: Option<String>,
        name: Option<String>,
        github_username: Option<String>,
    ) -> Result<User, UserError> {
        let new_user = build_new_user(github_id, email, name, github_username);
        let user = self.repo.create(new_user).await?;
        Ok(user)
    }

    #[instrument(name = "domain.user.create_from_github_login_in_op", skip(self, op))]
    pub async fn create_from_github_login_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        github_id: impl Into<String> + std::fmt::Debug,
        email: Option<String>,
        name: Option<String>,
        github_username: Option<String>,
    ) -> Result<User, UserError> {
        let new_user = build_new_user(github_id, email, name, github_username);
        let user = self.repo.create_in_op(op, new_user).await?;
        Ok(user)
    }

    #[instrument(name = "domain.user.find_by_id", skip(self))]
    pub async fn find_by_id(
        &self,
        id: impl Into<UserId> + std::fmt::Debug,
    ) -> Result<User, UserError> {
        Ok(self.repo.find_by_id(id.into()).await?)
    }

    /// Build the [`CommitAttribution`] for the current request and stash
    /// it in the request's `EventContext` so library post-persist hooks
    /// (Notes / Skills / Workflows) pick it up.
    ///
    /// JIT-resolves the human's GitHub identity for the
    /// `Co-Authored-By:` line: when an `acting_user_id` or
    /// `on_behalf_of_user_id` is present, looks the user up once.
    /// Failures degrade to a userless attribution rather than failing
    /// the write.
    #[instrument(name = "domain.user.commit_attribution", skip(self))]
    pub async fn commit_attribution(&self) -> CommitAttribution {
        let ctx = match Audit::collect_context() {
            Some(c) => c,
            None => {
                let attribution = CommitAttribution::library_default();
                attribution.put_in_event_context();
                return attribution;
            }
        };

        let human_user_id = ctx.on_behalf_of_user_id.or(ctx.acting_user_id);
        let human = match human_user_id {
            Some(uid) => match self.find_by_id(uid).await {
                Ok(u) => Some(u),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        user_id = %uid,
                        "commit_attribution: user lookup failed; degrading to userless attribution"
                    );
                    None
                }
            },
            None => None,
        };

        let kind = subject_kind(&ctx);
        let (author_name, author_email, committer_name, committer_email) =
            signatures_for(kind, human.as_ref());

        let mut attribution = CommitAttribution {
            author_name,
            author_email,
            committer_name,
            committer_email,
            kind,
            trailers: Vec::new(),
            co_authored_by: Vec::new(),
        };

        attribution.add_trailer("Drua-Subject-Type", kind.as_trailer_value());

        if let Some(user) = human.as_ref() {
            let (display, email) = github_co_author(user);
            attribution.add_co_authored_by(display, email);
            attribution.add_trailer("Drua-Acting-User", user.id.to_string());
        }

        for (audit_key, trailer_key) in [
            ("project_id", "Drua-Project"),
            ("workflow_id", "Drua-Workflow-Definition"),
            ("workflow_run_id", "Drua-Workflow-Run"),
            ("agent_id", "Drua-Agent"),
            ("sandbox_id", "Drua-Sandbox"),
            ("space_id", "Drua-Space"),
        ] {
            if let Some(value) = ctx.resource_ids.get(audit_key).and_then(|v| v.as_str()) {
                attribution.add_trailer(trailer_key, value);
            }
        }

        if let Some(agent_id) = ctx.acting_agent_id {
            attribution.add_trailer("Drua-Acting-Agent", agent_id.to_string());
        }
        if let Some(action) = ctx.action.as_deref() {
            attribution.add_trailer("Drua-Action", action);
        }
        if let Some(entrypoint) = ctx.entrypoint.as_deref() {
            attribution.add_trailer("Drua-Entrypoint", entrypoint);
        }

        attribution.put_in_event_context();
        attribution
    }
}

fn subject_kind(ctx: &AuditContextData) -> CommitSubjectKind {
    if ctx.resource_ids.contains_key("workflow_run_id") {
        return CommitSubjectKind::WorkflowAgent;
    }
    match (
        ctx.acting_user_id,
        ctx.acting_agent_id,
        ctx.on_behalf_of_user_id,
    ) {
        (_, Some(_), Some(_)) => CommitSubjectKind::AgentOnBehalfOfUser,
        (_, Some(_), None) => CommitSubjectKind::WorkflowAgent,
        (Some(_), None, _) => CommitSubjectKind::User,
        (None, None, Some(_)) => CommitSubjectKind::AgentOnBehalfOfUser,
        (None, None, None) => CommitSubjectKind::System,
    }
}

fn signatures_for(
    kind: CommitSubjectKind,
    human: Option<&User>,
) -> (String, String, String, String) {
    use drua_library::attribution::{
        ADMIN_BOT_EMAIL, ADMIN_BOT_NAME, AGENT_BOT_EMAIL, AGENT_BOT_NAME, LIBRARY_BOT_EMAIL,
        LIBRARY_BOT_NAME, SYSTEM_BOT_EMAIL, SYSTEM_BOT_NAME, WORKFLOW_BOT_EMAIL, WORKFLOW_BOT_NAME,
    };
    match kind {
        CommitSubjectKind::System => (
            SYSTEM_BOT_NAME.to_owned(),
            SYSTEM_BOT_EMAIL.to_owned(),
            SYSTEM_BOT_NAME.to_owned(),
            SYSTEM_BOT_EMAIL.to_owned(),
        ),
        CommitSubjectKind::Library => (
            LIBRARY_BOT_NAME.to_owned(),
            LIBRARY_BOT_EMAIL.to_owned(),
            LIBRARY_BOT_NAME.to_owned(),
            LIBRARY_BOT_EMAIL.to_owned(),
        ),
        CommitSubjectKind::User | CommitSubjectKind::ExportedAgent => {
            let (display, email) = human
                .map(github_co_author)
                .unwrap_or_else(|| (ADMIN_BOT_NAME.to_owned(), ADMIN_BOT_EMAIL.to_owned()));
            (
                format!("{display} (via drua)"),
                email,
                ADMIN_BOT_NAME.to_owned(),
                ADMIN_BOT_EMAIL.to_owned(),
            )
        }
        CommitSubjectKind::AgentOnBehalfOfUser => {
            let (display, email) = human
                .map(github_co_author)
                .unwrap_or_else(|| (AGENT_BOT_NAME.to_owned(), AGENT_BOT_EMAIL.to_owned()));
            (
                format!("{display} (via drua)"),
                email,
                AGENT_BOT_NAME.to_owned(),
                AGENT_BOT_EMAIL.to_owned(),
            )
        }
        CommitSubjectKind::WorkflowAgent => (
            WORKFLOW_BOT_NAME.to_owned(),
            WORKFLOW_BOT_EMAIL.to_owned(),
            WORKFLOW_BOT_NAME.to_owned(),
            WORKFLOW_BOT_EMAIL.to_owned(),
        ),
    }
}

/// Render a `(display_name, email)` pair for a `Co-Authored-By:` line.
/// Email keys on the immutable GitHub user id (form
/// `<id>+<username>@users.noreply.github.com`) so renames don't break
/// avatar lookup.
fn github_co_author(user: &User) -> (String, String) {
    let display = user
        .name
        .clone()
        .or_else(|| user.github_username.clone())
        .unwrap_or_else(|| format!("github:{}", user.github_id));
    let username = user.github_username.as_deref().unwrap_or("anonymous");
    let email = format!(
        "{id}+{username}@users.noreply.github.com",
        id = user.github_id,
        username = username,
    );
    (display, email)
}

fn build_new_user(
    github_id: impl Into<String>,
    email: Option<String>,
    name: Option<String>,
    github_username: Option<String>,
) -> NewUser {
    let mut builder = NewUser::builder();
    builder.github_id(github_id);
    if let Some(email) = email {
        builder.email(email);
    }
    if let Some(name) = name {
        builder.name(name);
    }
    if let Some(github_username) = github_username {
        builder.github_username(github_username);
    }
    builder.build().expect("Could not build new user")
}
