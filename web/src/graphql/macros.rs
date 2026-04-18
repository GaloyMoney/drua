/// Extract the `App` handle and `AuthSubject` from an async-graphql
/// [`Context`].
///
/// Instead of:
/// ```ignore
/// async fn workspaces(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<Workspace>> {
///     let app = ctx.data_unchecked::<App>();
///     let sub: &AuthSubject = ctx.data()?;
/// ```
///
/// Use:
/// ```ignore
/// async fn workspaces(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<Workspace>> {
///     let (app, sub) = app_and_sub_from_ctx!(ctx);
/// ```
#[macro_export]
macro_rules! app_and_sub_from_ctx {
    ($ctx:expr) => {{
        let app = $ctx.data_unchecked::<galoy_agents_core::App>();
        let sub: &galoy_agents_core::auth::AuthSubject = $ctx.data()?;
        (app, sub)
    }};
}

/// Standard mutation payload: a single-field struct wrapping the returned
/// GraphQL entity plus a `From` impl for ergonomic conversion.
///
/// ```ignore
/// mutation_payload! { WorkspaceCreatePayload, workspace: Workspace }
/// ```
///
/// Expands to:
/// ```ignore
/// #[derive(SimpleObject)]
/// pub struct WorkspaceCreatePayload {
///     workspace: Workspace,
/// }
/// impl From<Workspace> for WorkspaceCreatePayload { .. }
/// ```
/// Cursor-based list query following the Relay connection spec.
///
/// ```ignore
/// list_with_cursor!(
///     WorkspaceByCreatedAtCursor,
///     Workspace,
///     after,
///     first,
///     |query| app.workspaces().list(sub, query, direction)
/// )
/// ```
#[macro_export]
macro_rules! list_with_cursor {
    ($cursor:ty, $entity:ty, $after:expr, $first:expr, $load:expr) => {{
        async_graphql::types::connection::query(
            $after,
            None,
            Some($first),
            None,
            |after, _, first, _| async move {
                let first = first.expect("First always exists") as usize;
                let args = es_entity::PaginatedQueryArgs { first, after };
                let res = $load(args).await?;
                let mut connection =
                    async_graphql::types::connection::Connection::new(false, res.has_next_page);
                connection
                    .edges
                    .extend(res.entities.into_iter().map(|entity| {
                        let cursor = <$cursor>::from(&entity);
                        async_graphql::types::connection::Edge::new(
                            cursor,
                            <$entity>::from(entity),
                        )
                    }));
                Ok::<_, async_graphql::Error>(connection)
            },
        )
        .await
    }};
}

#[macro_export]
macro_rules! mutation_payload {
    ($payload:ident, $name:ident: $gql_type:ty) => {
        #[derive(async_graphql::SimpleObject)]
        pub struct $payload {
            $name: $gql_type,
        }

        impl From<$gql_type> for $payload {
            fn from($name: $gql_type) -> Self {
                Self { $name }
            }
        }
    };
}
