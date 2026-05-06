use std::sync::Arc;

use async_graphql::{ComplexObject, Context, InputObject, SimpleObject};

use super::agent::{Agent, ModelChain};
use super::mcp_creds::McpCreds;
use super::primitives::*;
use super::project_secret::ProjectSecret;
use super::sandbox::Sandbox;
use super::skill::Skill;

use drua_core::project::Project as DomainProject;

#[derive(SimpleObject, Clone)]
#[graphql(complex)]
pub struct Project {
    id: ProjectId,
    name: String,
    description: Option<String>,

    #[graphql(skip)]
    pub(super) entity: Arc<DomainProject>,
}

#[ComplexObject]
impl Project {
    async fn created_at(&self) -> Timestamp {
        self.entity.created_at().into()
    }

    /// Per-project chain. Inherited by every non-workflow agent in
    /// the project unless the agent has its own override.
    async fn model_chain_override(&self) -> Option<ModelChain> {
        self.entity.model_chain_override.clone().map(Into::into)
    }

    async fn lead(&self, ctx: &Context<'_>) -> async_graphql::Result<Agent> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let agent = app
            .agents()
            .find_by_id(sub, self.entity.lead_agent_id)
            .await?;
        Ok(Agent::from(agent))
    }

    /// Lead agent first.
    async fn agents(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<Agent>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let mut agents = app.agents().list_for_project(sub, self.entity.id).await?;
        let lead_id = self.entity.lead_agent_id;
        agents.sort_by_key(|a| if a.id == lead_id { 0 } else { 1 });
        Ok(agents.into_iter().map(Agent::from).collect())
    }

    async fn sandboxes(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<Sandbox>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let sandboxes = app
            .sandboxes()
            .list_for_project(sub, self.entity.id)
            .await?;
        Ok(sandboxes.into_iter().map(Sandbox::from).collect())
    }

    async fn skills(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<Skill>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let skills = app.skills().list_by_project_id(sub, self.entity.id).await?;
        Ok(skills.into_iter().map(Skill::from).collect())
    }

    /// Metadata only — values are never exposed via GraphQL.
    async fn secrets(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<ProjectSecret>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let secrets = app
            .project_secrets()
            .list_by_project(sub, self.entity.id)
            .await?;
        Ok(secrets.into_iter().map(ProjectSecret::from).collect())
    }

    async fn mcp_credentials(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<McpCreds>> {
        let (app, sub) = app_and_sub_from_ctx!(ctx);
        let user_id = sub
            .originating_user_id()
            .ok_or_else(|| async_graphql::Error::new("Authentication required"))?;
        let creds = app.mcp_creds().list_all_for_user(sub, user_id).await?;
        Ok(creds.into_iter().map(McpCreds::from).collect())
    }
}

impl From<DomainProject> for Project {
    fn from(entity: DomainProject) -> Self {
        Self {
            id: entity.id,
            name: entity.name.clone(),
            description: entity.description.clone(),
            entity: Arc::new(entity),
        }
    }
}

#[derive(InputObject)]
pub struct ProjectCreateInput {
    pub name: String,
    pub description: Option<String>,
}

mutation_payload! { ProjectCreatePayload, project: Project }

#[derive(InputObject)]
pub struct ProjectUpdateInput {
    pub id: ProjectId,
    pub description: Option<String>,
}

mutation_payload! { ProjectUpdatePayload, project: Project }

#[derive(InputObject)]
pub struct ProjectDeleteInput {
    pub id: ProjectId,
}

mutation_payload! { ProjectDeletePayload, project: Project }

#[derive(InputObject)]
pub struct ProjectUpdateModelChainInput {
    pub id: ProjectId,
    /// `None` clears the override.
    pub chain: Option<ModelChain>,
}

mutation_payload! { ProjectUpdateModelChainPayload, project: Project }
