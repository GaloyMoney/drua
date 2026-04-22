#[macro_use]
pub(crate) mod macros;
mod agent;
mod mutation;
pub(crate) mod primitives;
mod query;
mod session;
mod subscription;
mod types;
mod workspace;

use std::convert::Infallible;

use async_graphql::{extensions, Schema};
use async_graphql_axum::{GraphQLRequest, GraphQLResponse};
use axum::{
    extract::State,
    response::{
        sse::{Event, Sse},
        IntoResponse,
    },
    routing::post,
    Extension, Json, Router,
};
use tokio_stream::{self as stream, StreamExt};

use crate::AppState;

pub use mutation::Mutation;
pub use primitives::Yaml;
pub use query::Query;
pub use subscription::Subscription;

pub struct AppConfigYaml(pub Yaml);

pub type AgentsSchema = Schema<Query, Mutation, Subscription>;

/// Build the GraphQL schema.
///
/// Follows lana-bank's pattern: `app` is optional so that the `write_sdl`
/// binary can generate the SDL without a live database connection.
pub fn schema(app: Option<domain::App>) -> AgentsSchema {
    let mut builder = Schema::build(Query, Mutation, Subscription).extension(extensions::Tracing);

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
        .route("/graphql/stream", post(graphql_sse_handler))
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

    // Inject the serialized app config YAML so the `appConfig` query can return it.
    request = request.data(AppConfigYaml(state.app_config_yaml.clone()));

    // Override the REST middleware's generic "api: POST /graphql" entrypoint
    // with the concrete operation name so audit entries are useful.
    let op_name = request.operation_name.as_deref().unwrap_or("anonymous");
    drua_core::audit::Audit::record_entrypoint(format!("graphql: {}", op_name));

    schema.execute(request).await.into()
}

/// SSE handler for GraphQL subscriptions.
///
/// Clients POST a standard GraphQL request body (containing a subscription
/// query) and receive an SSE stream of `next` events, followed by a final
/// `complete` event. Same pattern as lana-bank's admin-server.
async fn graphql_sse_handler(
    State(state): State<AppState>,
    Extension(schema): Extension<AgentsSchema>,
    auth: Option<Extension<domain::auth::AuthSubject>>,
    Json(req): Json<async_graphql::Request>,
) -> impl IntoResponse {
    let auth_subject = auth
        .map(|Extension(sub)| sub)
        .unwrap_or(domain::auth::AuthSubject::Anonymous);

    let request = req.data(state.app.clone()).data(auth_subject);

    let responses = schema.execute_stream(request);

    let events = responses.map(|r| {
        Ok::<_, Infallible>(
            Event::default()
                .event("next")
                .json_data(&r)
                .unwrap_or_else(|_| {
                    Event::default()
                        .event("next")
                        .data(r#"{"errors":[{"message":"Serialization error"}]}"#)
                }),
        )
    });

    let complete = stream::iter([Ok::<_, Infallible>(
        Event::default().event("complete").data(""),
    )]);

    Sse::new(events.chain(complete))
}

use drua_core as domain;
