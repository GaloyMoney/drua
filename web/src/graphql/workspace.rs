use std::sync::Arc;

use async_graphql::{ComplexObject, InputObject, SimpleObject};

use super::primitives::*;

use galoy_agents_core::workspace::Workspace as DomainWorkspace;

#[derive(SimpleObject, Clone)]
#[graphql(complex)]
pub struct Workspace {
    id: WorkspaceId,
    name: String,
    description: Option<String>,

    #[graphql(skip)]
    pub(super) entity: Arc<DomainWorkspace>,
}

#[ComplexObject]
impl Workspace {
    async fn created_at(&self) -> Timestamp {
        self.entity.created_at().into()
    }

}

impl From<DomainWorkspace> for Workspace {
    fn from(entity: DomainWorkspace) -> Self {
        Self {
            id: entity.id,
            name: entity.name.clone(),
            description: entity.description.clone(),
            entity: Arc::new(entity),
        }
    }
}

#[derive(InputObject)]
pub struct WorkspaceCreateInput {
    pub name: String,
    pub description: Option<String>,
}

mutation_payload! { WorkspaceCreatePayload, workspace: Workspace }

#[derive(InputObject)]
pub struct WorkspaceUpdateInput {
    pub id: WorkspaceId,
    pub name: String,
    pub description: Option<String>,
}

mutation_payload! { WorkspaceUpdatePayload, workspace: Workspace }

#[derive(InputObject)]
pub struct WorkspaceDeleteInput {
    pub id: WorkspaceId,
}

mutation_payload! { WorkspaceDeletePayload, workspace: Workspace }
