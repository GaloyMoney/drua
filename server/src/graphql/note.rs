use async_graphql::{InputObject, SimpleObject};

use super::primitives::*;

#[derive(InputObject)]
pub struct NoteDeleteInput {
    pub id: NoteId,
    pub project_id: ProjectId,
}

#[derive(SimpleObject)]
pub struct NoteDeletePayload {
    pub deleted_id: NoteId,
}
