use std::sync::Arc;

use async_graphql::{ComplexObject, InputObject, SimpleObject};

use super::primitives::*;

use drua_core::skill::Skill as DomainSkill;

#[derive(SimpleObject, Clone)]
#[graphql(complex)]
pub struct Skill {
    id: SkillId,
    workspace_id: WorkspaceId,
    name: String,
    description: String,
    body: String,

    #[graphql(skip)]
    entity: Arc<DomainSkill>,
}

#[ComplexObject]
impl Skill {
    async fn created_at(&self) -> Timestamp {
        self.entity.created_at().into()
    }
}

impl From<DomainSkill> for Skill {
    fn from(entity: DomainSkill) -> Self {
        Self {
            id: entity.id,
            workspace_id: entity.workspace_id,
            name: entity.name.clone(),
            description: entity.description.clone(),
            body: entity.body.clone(),
            entity: Arc::new(entity),
        }
    }
}

#[derive(InputObject)]
pub struct SkillCreateInput {
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub description: String,
    pub body: String,
}

mutation_payload! { SkillCreatePayload, skill: Skill }

#[derive(InputObject)]
pub struct SkillUpdateInput {
    pub id: SkillId,
    pub name: Option<String>,
    pub description: Option<String>,
    pub body: Option<String>,
}

mutation_payload! { SkillUpdatePayload, skill: Skill }

#[derive(InputObject)]
pub struct SkillDeleteInput {
    pub id: SkillId,
}

#[derive(async_graphql::SimpleObject)]
pub struct SkillDeletePayload {
    pub deleted_id: SkillId,
}
