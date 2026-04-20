#[macro_use]
pub(crate) mod macros;
mod agent;
mod mutation;
pub(crate) mod primitives;
mod query;
mod session;
mod types;
mod workspace;

use async_graphql::{extensions, EmptySubscription, Schema};
use async_graphql_axum::{GraphQLRequest, GraphQLResponse};
use axum::{extract::State, routing::post, Extension, Router};

use crate::AppState;

pub use mutation::Mutation;
pub use query::Query;

pub type AgentsSchema = Schema<Query, Mutation, EmptySubscription>;

/// Build the GraphQL schema.
///
/// Follows lana-bank's pattern: `app` is optional so that the `write_sdl`
/// binary can generate the SDL without a live database connection.
pub fn schema(app: Option<domain::App>) -> AgentsSchema {
    let mut builder =
        Schema::build(Query, Mutation, EmptySubscription).extension(extensions::Tracing);

    if let Some(app) = app {
        builder = builder.data(app);
    }

    builder.finish()
}

/// Axum router for the GraphQL endpoint.
pub fn router() -> Router<AppState> {
    let gql_schema = schema(None);

    Router::new()
        .route("/graphql", post(graphql_handler))
        .layer(Extension(gql_schema))
}

async fn graphql_handler(
    State(state): State<AppState>,
    Extension(schema): Extension<AgentsSchema>,
    auth: Option<Extension<domain::auth::AuthSubject>>,
    req: GraphQLRequest,
) -> GraphQLResponse {
    let mut request = req.into_inner();

    // Inject App from shared state so resolvers can access domain services.
    request = request.data(state.app.clone());

    // Inject the per-request auth subject resolved by the auth middleware.
    let auth_subject = auth
        .map(|Extension(sub)| sub)
        .unwrap_or(domain::auth::AuthSubject::Anonymous);
    request = request.data(auth_subject);

    // Override the REST middleware's generic "api: POST /graphql" entrypoint
    // with the concrete operation name so audit entries are useful.
    let op_name = request.operation_name.as_deref().unwrap_or("anonymous");
    drua_core::audit::Audit::record_entrypoint(format!("graphql: {}", op_name));

    schema.execute(request).await.into()
}

use drua_core as domain;
