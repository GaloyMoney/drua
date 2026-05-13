use async_graphql::{Context, Object};

use super::agent::*;
use super::mcp_creds::*;
use super::sandbox::*;

use super::note::*;
use super::project::*;
use super::project_secret::*;
use super::skill::*;
use super::workflow::*;

pub struct Mutation;

#[Object]
impl Mutation {
    async fn ping(&self) -> &str {
        "pong"
    }

    async fn project_create(
        &self,
        ctx: &Context<'_>,
        input: ProjectCreateInput,
    ) -> async_graphql::Result<ProjectCreatePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let project = app
            .projects()
            .create(sub, input.name, input.description)
            .await?;
        Ok(ProjectCreatePayload::from(Project::from(project)))
    }

    async fn project_update(
        &self,
        ctx: &Context<'_>,
        input: ProjectUpdateInput,
    ) -> async_graphql::Result<ProjectUpdatePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let project = app
            .projects()
            .update(sub, input.id, input.description)
            .await?;
        Ok(ProjectUpdatePayload::from(Project::from(project)))
    }

    async fn project_delete(
        &self,
        ctx: &Context<'_>,
        input: ProjectDeleteInput,
    ) -> async_graphql::Result<ProjectDeletePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let project = app.projects().delete(sub, input.id).await?;
        Ok(ProjectDeletePayload::from(Project::from(project)))
    }

    async fn project_update_model_chain(
        &self,
        ctx: &Context<'_>,
        input: ProjectUpdateModelChainInput,
    ) -> async_graphql::Result<ProjectUpdateModelChainPayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let project = app
            .projects()
            .update_model_chain(sub, input.id, input.chain.map(Into::into))
            .await?;
        Ok(ProjectUpdateModelChainPayload::from(Project::from(project)))
    }

    async fn agent_create(
        &self,
        ctx: &Context<'_>,
        input: AgentCreateInput,
    ) -> async_graphql::Result<AgentCreatePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let attach = match (input.sandbox_id, input.sandbox_mode) {
            (Some(sid), Some(mode)) => Some((sid, mode.into())),
            _ => None,
        };
        let agent = app
            .agents()
            .create_agent(
                sub,
                input.project_id,
                input.name,
                attach,
                input.model_chain.map(Into::into),
            )
            .await?;
        Ok(AgentCreatePayload::from(Agent::from(agent)))
    }

    async fn agent_update_model_chain(
        &self,
        ctx: &Context<'_>,
        input: AgentUpdateModelChainInput,
    ) -> async_graphql::Result<AgentUpdateModelChainPayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let agent = app
            .agents()
            .update_model_chain(sub, input.agent_id, input.chain.map(Into::into))
            .await?;
        Ok(AgentUpdateModelChainPayload::from(Agent::from(agent)))
    }

    async fn agent_attach_sandbox(
        &self,
        ctx: &Context<'_>,
        input: AgentAttachSandboxInput,
    ) -> async_graphql::Result<AgentAttachSandboxPayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let agent = app
            .agents()
            .attach_sandbox(sub, input.agent_id, input.sandbox_id, input.mode.into())
            .await?;
        Ok(AgentAttachSandboxPayload::from(Agent::from(agent)))
    }

    async fn agent_detach_sandbox(
        &self,
        ctx: &Context<'_>,
        input: AgentDetachSandboxInput,
    ) -> async_graphql::Result<AgentDetachSandboxPayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let agent = app
            .agents()
            .detach_sandbox(sub, input.agent_id, input.sandbox_id)
            .await?;
        Ok(AgentDetachSandboxPayload::from(Agent::from(agent)))
    }

    async fn agent_delete(
        &self,
        ctx: &Context<'_>,
        input: AgentDeleteInput,
    ) -> async_graphql::Result<AgentDeletePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        app.agents().delete(sub, input.id).await?;
        Ok(AgentDeletePayload {
            deleted_id: input.id,
        })
    }

    async fn sandbox_create(
        &self,
        ctx: &Context<'_>,
        input: SandboxCreateInput,
    ) -> async_graphql::Result<SandboxCreatePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let specs = drua_core::sandbox::SandboxSpecs {
            cpu: input.cpu,
            memory: input.memory,
            disk_size: input.disk_size,
        };
        let mode = match input.mode {
            SandboxCreateMode::Scratch => drua_core::sandbox::SandboxMode::Scratch,
            SandboxCreateMode::Repo => drua_core::sandbox::SandboxMode::Repo {
                repo_url: input.repo_url.unwrap_or_default(),
                branch: input.branch,
            },
        };
        let sb = app
            .sandboxes()
            .create(sub, input.project_id, input.name, specs, mode)
            .await?;
        Ok(SandboxCreatePayload::from(Sandbox::from(sb)))
    }

    async fn sandbox_suspend(
        &self,
        ctx: &Context<'_>,
        input: SandboxSuspendInput,
    ) -> async_graphql::Result<SandboxSuspendPayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let sb = app.sandboxes().suspend(sub, input.id).await?;
        Ok(SandboxSuspendPayload::from(Sandbox::from(sb)))
    }

    async fn sandbox_restart(
        &self,
        ctx: &Context<'_>,
        input: SandboxRestartInput,
    ) -> async_graphql::Result<SandboxRestartPayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let sb = app.sandboxes().restart(sub, input.id).await?;
        Ok(SandboxRestartPayload::from(Sandbox::from(sb)))
    }

    async fn project_secret_create(
        &self,
        ctx: &Context<'_>,
        input: ProjectSecretCreateInput,
    ) -> async_graphql::Result<ProjectSecretCreatePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let secret = app
            .project_secrets()
            .create(
                sub,
                input.project_id,
                &input.name,
                input.secret_type.into(),
                &input.value,
            )
            .await?;
        Ok(ProjectSecretCreatePayload::from(ProjectSecret::from(
            secret,
        )))
    }

    async fn project_secret_delete(
        &self,
        ctx: &Context<'_>,
        input: ProjectSecretDeleteInput,
    ) -> async_graphql::Result<ProjectSecretDeletePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        app.project_secrets().delete(sub, input.id).await?;
        Ok(ProjectSecretDeletePayload {
            deleted_id: input.id,
        })
    }

    async fn skill_delete(
        &self,
        ctx: &Context<'_>,
        input: SkillDeleteInput,
    ) -> async_graphql::Result<SkillDeletePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        app.skills().delete(sub, input.id, input.project_id).await?;
        Ok(SkillDeletePayload {
            deleted_id: input.id,
        })
    }

    async fn note_delete(
        &self,
        ctx: &Context<'_>,
        input: NoteDeleteInput,
    ) -> async_graphql::Result<NoteDeletePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        app.notes().delete(sub, input.project_id, input.id).await?;
        Ok(NoteDeletePayload {
            deleted_id: input.id,
        })
    }

    async fn workflow_delete(
        &self,
        ctx: &Context<'_>,
        input: WorkflowDeleteInput,
    ) -> async_graphql::Result<WorkflowDeletePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        app.workflows().delete(sub, input.id).await?;
        Ok(WorkflowDeletePayload {
            deleted_id: input.id,
        })
    }

    async fn mcp_credentials_create(
        &self,
        ctx: &Context<'_>,
        input: McpCredentialsCreateInput,
    ) -> async_graphql::Result<McpCredentialsCreatePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let user_id = sub
            .originating_user_id()
            .ok_or_else(|| async_graphql::Error::new("Authentication required"))?;

        let scopes: Vec<drua_core::auth::AuthScope> =
            input.scopes.iter().map(|s| s.as_str().into()).collect();

        let (raw_token, token_hash) = drua_core::mcp_creds::token::generate_token();

        let creds = app
            .mcp_creds()
            .create_for_user(sub, user_id, input.name, token_hash, scopes)
            .await?;

        Ok(McpCredentialsCreatePayload {
            mcp_creds: McpCreds::from(creds),
            token: raw_token,
        })
    }

    async fn mcp_credentials_revoke(
        &self,
        ctx: &Context<'_>,
        input: McpCredentialsRevokeInput,
    ) -> async_graphql::Result<McpCredentialsRevokePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let user_id = sub
            .originating_user_id()
            .ok_or_else(|| async_graphql::Error::new("Authentication required"))?;
        let creds = app.mcp_creds().revoke(sub, user_id, input.id).await?;
        Ok(McpCredentialsRevokePayload::from(McpCreds::from(creds)))
    }
}
