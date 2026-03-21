mod auth;
mod types;

use async_graphql::*;
use sha2::{Digest, Sha256};
use tracing::instrument;

pub use auth::*;
pub use types::*;

use crate::agent::Agents;
use crate::primitives::*;
use crate::user::Users;

pub struct Query;

#[Object]
impl Query {
    #[instrument(name = "mcp_gateway.graphql.me", skip_all)]
    async fn me(&self, ctx: &Context<'_>) -> async_graphql::Result<UserType> {
        let auth = ctx.data::<AuthContext>()?;
        let user_id = auth.user_id()?;

        let users = ctx.data::<Users>()?;
        let user = users.find_by_id(user_id).await?;

        Ok(UserType::from(user))
    }

    #[instrument(name = "mcp_gateway.graphql.my_agents", skip_all)]
    async fn my_agents(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<AgentType>> {
        let auth = ctx.data::<AuthContext>()?;
        let user_id = auth.user_id()?;

        let agents = ctx.data::<Agents>()?;
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = agents
            .list_for_user(user_id, query, es_entity::ListDirection::Descending)
            .await?;

        Ok(result.entities.into_iter().map(AgentType::from).collect())
    }
}

pub struct Mutation;

#[Object]
impl Mutation {
    #[instrument(name = "mcp_gateway.graphql.create_agent", skip_all)]
    async fn create_agent(
        &self,
        ctx: &Context<'_>,
        input: CreateAgentInput,
    ) -> async_graphql::Result<CreateAgentPayload> {
        let auth = ctx.data::<AuthContext>()?;
        let user_id = auth.user_id()?;

        let raw_token = format!("gla_{}", uuid::Uuid::new_v4().simple());
        let token_hash = format!("{:x}", Sha256::digest(raw_token.as_bytes()));

        let agents = ctx.data::<Agents>()?;
        let agent = agents
            .create_for_user(user_id, input.name, token_hash, input.scopes)
            .await?;

        Ok(CreateAgentPayload {
            agent: AgentType::from(&agent),
            token: raw_token,
        })
    }

    #[instrument(name = "mcp_gateway.graphql.revoke_agent", skip_all)]
    async fn revoke_agent(
        &self,
        ctx: &Context<'_>,
        agent_id: ID,
    ) -> async_graphql::Result<RevokeAgentPayload> {
        let auth = ctx.data::<AuthContext>()?;
        let user_id = auth.user_id()?;

        let id: AgentId = agent_id
            .parse::<uuid::Uuid>()
            .map_err(|e| async_graphql::Error::new(format!("Invalid agent ID: {e}")))?
            .into();

        let agents = ctx.data::<Agents>()?;
        let agent = agents.find_by_id(id).await?;

        if agent.user_id != user_id {
            return Err(async_graphql::Error::new("Agent not found"));
        }

        let agent = agents.revoke(id).await?;

        Ok(RevokeAgentPayload {
            agent: AgentType::from(&agent),
        })
    }
}

#[derive(InputObject)]
pub struct CreateAgentInput {
    pub name: String,
    pub scopes: Vec<String>,
}

pub type McpGatewaySchema = Schema<Query, Mutation, EmptySubscription>;

pub fn schema(users: Users, agents: Agents) -> McpGatewaySchema {
    Schema::build(Query, Mutation, EmptySubscription)
        .data(users)
        .data(agents)
        .finish()
}

pub async fn graphql_handler(
    schema: axum::Extension<McpGatewaySchema>,
    req: async_graphql_axum::GraphQLRequest,
) -> async_graphql_axum::GraphQLResponse {
    let mut req = req.into_inner();
    req = req.data(AuthContext::Anonymous);
    schema.execute(req).await.into()
}

pub async fn graphql_playground() -> impl axum::response::IntoResponse {
    axum::response::Html(
        async_graphql::http::GraphiQLSource::build()
            .endpoint("/graphql")
            .finish(),
    )
}
