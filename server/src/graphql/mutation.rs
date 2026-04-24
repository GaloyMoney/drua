use async_graphql::{Context, Object};

use super::agent::*;
use super::mcp_creds::*;
use super::sandbox::*;
use super::skill::*;
use super::workspace::*;
use super::workspace_secret::*;

pub struct Mutation;

#[Object]
impl Mutation {
    async fn ping(&self) -> &str {
        "pong"
    }

    // ── Workspace ───────────────────────────────────────────────────────

    async fn workspace_create(
        &self,
        ctx: &Context<'_>,
        input: WorkspaceCreateInput,
    ) -> async_graphql::Result<WorkspaceCreatePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let ws = app
            .workspaces()
            .create(sub, input.name, input.description)
            .await?;
        Ok(WorkspaceCreatePayload::from(Workspace::from(ws)))
    }

    async fn workspace_update(
        &self,
        ctx: &Context<'_>,
        input: WorkspaceUpdateInput,
    ) -> async_graphql::Result<WorkspaceUpdatePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let ws = app
            .workspaces()
            .update(sub, input.id, input.description)
            .await?;
        Ok(WorkspaceUpdatePayload::from(Workspace::from(ws)))
    }

    async fn workspace_delete(
        &self,
        ctx: &Context<'_>,
        input: WorkspaceDeleteInput,
    ) -> async_graphql::Result<WorkspaceDeletePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let ws = app.workspaces().delete(sub, input.id).await?;
        Ok(WorkspaceDeletePayload::from(Workspace::from(ws)))
    }

    // ── Agent ───────────────────────────────────────────────────────────

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
            .create_agent(sub, input.workspace_id, input.name, attach)
            .await?;
        Ok(AgentCreatePayload::from(Agent::from(agent)))
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

    // ── Sandbox ─────────────────────────────────────────────────────────

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
            .create(sub, input.workspace_id, input.name, specs, mode)
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

    // ── Skill ───────────────────────────────────────────────────────────

    async fn skill_create(
        &self,
        ctx: &Context<'_>,
        input: SkillCreateInput,
    ) -> async_graphql::Result<SkillCreatePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let new = drua_core::skill::NewSkill::builder()
            .workspace_id(input.workspace_id)
            .name(input.name)
            .description(input.description)
            .body(input.body)
            .build()
            .map_err(|e| async_graphql::Error::new(e.to_string()))?;
        let skill = app.skills().create(sub, new).await?;
        Ok(SkillCreatePayload::from(Skill::from(skill)))
    }

    async fn skill_update(
        &self,
        ctx: &Context<'_>,
        input: SkillUpdateInput,
    ) -> async_graphql::Result<SkillUpdatePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let mut skill = app.skills().find_by_id(sub, input.id).await?;
        let _ = skill.update(input.name, input.description, input.body);
        app.skills().update(sub, &mut skill).await?;
        Ok(SkillUpdatePayload::from(Skill::from(skill)))
    }

    async fn skill_delete(
        &self,
        ctx: &Context<'_>,
        input: SkillDeleteInput,
    ) -> async_graphql::Result<SkillDeletePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        app.skills().delete(sub, input.id).await?;
        Ok(SkillDeletePayload {
            deleted_id: input.id,
        })
    }

    // ── Workspace Secret ────────────────────────────────────────────────

    async fn workspace_secret_create(
        &self,
        ctx: &Context<'_>,
        input: WorkspaceSecretCreateInput,
    ) -> async_graphql::Result<WorkspaceSecretCreatePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let secret = app
            .workspace_secrets()
            .create(
                sub,
                input.workspace_id,
                &input.name,
                input.secret_type.into(),
                &input.value,
            )
            .await?;
        Ok(WorkspaceSecretCreatePayload::from(WorkspaceSecret::from(
            secret,
        )))
    }

    async fn workspace_secret_delete(
        &self,
        ctx: &Context<'_>,
        input: WorkspaceSecretDeleteInput,
    ) -> async_graphql::Result<WorkspaceSecretDeletePayload> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        app.workspace_secrets().delete(sub, input.id).await?;
        Ok(WorkspaceSecretDeletePayload {
            deleted_id: input.id,
        })
    }

    // ── MCP Credentials ─────────────────────────────────────────────────

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
