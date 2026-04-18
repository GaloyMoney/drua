use async_graphql::Object;

pub struct Mutation;

#[Object]
impl Mutation {
    /// Placeholder — will be replaced with real mutations.
    async fn ping(&self) -> &str {
        "pong"
    }
}
