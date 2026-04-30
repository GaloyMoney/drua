use std::sync::Arc;

use async_graphql::{ComplexObject, SimpleObject};

use super::primitives::*;

use drua_core::skill::Skill as DomainSkill;

#[derive(SimpleObject, Clone)]
#[graphql(complex)]
pub struct Skill {
    id: SkillId,
    project_id: Option<ProjectId>,
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
            project_id: entity.project_id,
            name: entity.name.clone(),
            description: entity.description.clone(),
            body: entity.body.clone(),
            entity: Arc::new(entity),
        }
    }
}
