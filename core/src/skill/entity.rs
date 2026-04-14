use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::primitives::*;

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "SkillId")]
pub enum SkillEvent {
    Initialized {
        id: SkillId,
        workspace_id: WorkspaceId,
        name: String,
        description: String,
        body: String,
    },
    Updated {
        name: Option<String>,
        description: Option<String>,
        body: Option<String>,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Skill {
    pub id: SkillId,
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub description: String,
    pub body: String,
    events: EntityEvents<SkillEvent>,
}

impl Skill {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }

    pub fn update(
        &mut self,
        name: Option<String>,
        description: Option<String>,
        body: Option<String>,
    ) -> Idempotent<()> {
        if let Some(ref n) = name {
            self.name = n.clone();
        }
        if let Some(ref d) = description {
            self.description = d.clone();
        }
        if let Some(ref b) = body {
            self.body = b.clone();
        }
        self.events.push(SkillEvent::Updated {
            name,
            description,
            body,
        });
        Idempotent::Executed(())
    }
}

impl core::fmt::Display for Skill {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Skill: {}, name: {}", self.id, self.name)
    }
}

impl TryFromEvents<SkillEvent> for Skill {
    fn try_from_events(events: EntityEvents<SkillEvent>) -> Result<Self, EntityHydrationError> {
        let mut builder = SkillBuilder::default();

        for event in events.iter_all() {
            match event {
                SkillEvent::Initialized {
                    id,
                    workspace_id,
                    name,
                    description,
                    body,
                } => {
                    builder = builder
                        .id(*id)
                        .workspace_id(*workspace_id)
                        .name(name.clone())
                        .description(description.clone())
                        .body(body.clone());
                }
                SkillEvent::Updated {
                    name,
                    description,
                    body,
                } => {
                    if let Some(name) = name {
                        builder = builder.name(name.clone());
                    }
                    if let Some(description) = description {
                        builder = builder.description(description.clone());
                    }
                    if let Some(body) = body {
                        builder = builder.body(body.clone());
                    }
                }
            }
        }

        builder.events(events).build()
    }
}

#[derive(Debug, Builder)]
#[builder(pattern = "owned")]
pub struct NewSkill {
    #[builder(setter(into))]
    pub(super) id: SkillId,
    #[builder(setter(into))]
    pub(super) workspace_id: WorkspaceId,
    #[builder(setter(into))]
    pub(super) name: String,
    #[builder(setter(into))]
    pub(super) description: String,
    #[builder(setter(into))]
    pub(super) body: String,
}

impl NewSkill {
    pub fn builder() -> NewSkillBuilder {
        NewSkillBuilder::default().id(SkillId::new())
    }
}

impl IntoEvents<SkillEvent> for NewSkill {
    fn into_events(self) -> EntityEvents<SkillEvent> {
        EntityEvents::init(
            self.id,
            [SkillEvent::Initialized {
                id: self.id,
                workspace_id: self.workspace_id,
                name: self.name,
                description: self.description,
                body: self.body,
            }],
        )
    }
}
