use async_graphql::{InputObject, SimpleObject};

use super::primitives::*;

#[derive(InputObject)]
pub struct WorkflowDeleteInput {
    pub id: WorkflowDefinitionId,
}

#[derive(SimpleObject)]
pub struct WorkflowDeletePayload {
    pub deleted_id: WorkflowDefinitionId,
}
