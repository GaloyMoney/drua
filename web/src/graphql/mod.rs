mod mutation;
mod query;
mod types;

use async_graphql::{extensions, EmptySubscription, Schema};
use async_graphql_axum::{GraphQLRequest, GraphQLResponse};
use axum::{
    response::{Html, IntoResponse},
    routing::{get, post},
    Extension, Router,
};

pub use mutation::Mutation;
pub use query::Query;

pub type AgentsSchema = Schema<Query, Mutation, EmptySubscription>;

/// Build the GraphQL schema with the tracing extension enabled.
///
/// Follows lana-bank's pattern: callers can later inject application
/// data via `.data()` before calling `.finish()`, but this minimal
/// version builds a self-contained schema suitable for the `ping`
/// placeholder queries.
pub fn schema() -> AgentsSchema {
    Schema::build(Query, Mutation, EmptySubscription)
        .extension(extensions::Tracing)
        .finish()
}

/// Axum router for the GraphQL endpoint and playground.
///
/// **Not yet mounted** — call `.merge(graphql::router())` from the
/// top-level router when ready to expose the API.
pub fn router() -> Router {
    let schema = schema();

    Router::new()
        .route("/graphql", post(graphql_handler))
        .route("/graphql/playground", get(graphql_playground))
        .layer(Extension(schema))
}

async fn graphql_handler(
    Extension(schema): Extension<AgentsSchema>,
    req: GraphQLRequest,
) -> GraphQLResponse {
    schema.execute(req.into_inner()).await.into()
}

async fn graphql_playground() -> impl IntoResponse {
    Html(async_graphql::http::playground_source(
        async_graphql::http::GraphQLPlaygroundConfig::new("/graphql"),
    ))
}
