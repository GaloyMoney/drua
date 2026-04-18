use async_graphql::{Context, Object};

use super::primitives::*;

pub struct Query;

#[Object]
impl Query {
    async fn ping(&self) -> &str {
        "pong"
    }

    /// The ID of the currently authenticated user, if any.
    async fn me(&self, ctx: &Context<'_>) -> async_graphql::Result<Option<UUID>> {
        let (_app, sub) = app_and_sub_from_ctx!(ctx);
        match sub.originating_user_id() {
            Some(id) => Ok(Some(UUID::from(uuid::Uuid::from(id)))),
            None => Ok(None),
        }
    }
}
