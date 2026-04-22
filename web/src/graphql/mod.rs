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

use async_graphql::{extensions, Data, Schema};
use async_graphql_axum::{GraphQLProtocol, GraphQLRequest, GraphQLResponse, GraphQLWebSocket};
use axum::{
    extract::{ws::WebSocketUpgrade, State},
    response::Response,
    routing::{get, post},
    Extension, Router,
};

use crate::AppState;

pub use mutation::Mutation;
pub use query::Query;
pub use subscription::Subscription;

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
        .route("/graphql/ws", get(graphql_ws_handler))
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

/// WebSocket handler for GraphQL subscriptions (graphql-ws protocol).
///
/// Auth is resolved from the HTTP upgrade request by the auth middleware,
/// then injected into the subscription context via `on_connection_init`.
async fn graphql_ws_handler(
    State(state): State<AppState>,
    Extension(schema): Extension<AgentsSchema>,
    auth: Option<Extension<domain::auth::AuthSubject>>,
    protocol: GraphQLProtocol,
    ws: WebSocketUpgrade,
) -> Response {
    let app = state.app.clone();
    let auth_subject = auth
        .map(|Extension(sub)| sub)
        .unwrap_or(domain::auth::AuthSubject::Anonymous);

    ws.protocols(["graphql-transport-ws", "graphql-ws"])
        .on_upgrade(move |socket| async move {
            GraphQLWebSocket::new(socket, schema, protocol)
                .on_connection_init(move |_| async move {
                    let mut data = Data::default();
                    data.insert(app);
                    data.insert(auth_subject);
                    Ok(data)
                })
                .serve()
                .await
        })
}

use drua_core as domain;
