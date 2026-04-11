use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::encryption::Encrypted;
use crate::primitives::*;

use super::primitives::SecretType;

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "WorkspaceSecretId")]
pub enum WorkspaceSecretEvent {
    Initialized {
        id: WorkspaceSecretId,
        workspace_id: WorkspaceId,
        name: String,
        secret_type: SecretType,
        encrypted_value: Encrypted,
    },
    ValueUpdated {
        encrypted_value: Encrypted,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct WorkspaceSecret {
    pub id: WorkspaceSecretId,
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub secret_type: SecretType,
    pub(super) encrypted_value: Encrypted,
    events: EntityEvents<WorkspaceSecretEvent>,
}

impl WorkspaceSecret {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }

    pub(super) fn update_value(&mut self, encrypted_value: Encrypted) -> Idempotent<()> {
        self.encrypted_value = encrypted_value.clone();
        self.events
            .push(WorkspaceSecretEvent::ValueUpdated { encrypted_value });
        Idempotent::Executed(())
    }

    /// Access the encrypted value (for decryption by the service layer).
    pub(super) fn encrypted_value(&self) -> &Encrypted {
        &self.encrypted_value
    }
}

impl core::fmt::Display for WorkspaceSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "WorkspaceSecret: {}, name: {}, type: {}",
            self.id, self.name, self.secret_type
        )
    }
}

impl TryFromEvents<WorkspaceSecretEvent> for WorkspaceSecret {
    fn try_from_events(
        events: EntityEvents<WorkspaceSecretEvent>,
    ) -> Result<Self, EntityHydrationError> {
        let mut builder = WorkspaceSecretBuilder::default();

        for event in events.iter_all() {
            match event {
                WorkspaceSecretEvent::Initialized {
                    id,
                    workspace_id,
                    name,
                    secret_type,
                    encrypted_value,
                } => {
                    builder = builder
                        .id(*id)
                        .workspace_id(*workspace_id)
                        .name(name.clone())
                        .secret_type(*secret_type)
                        .encrypted_value(encrypted_value.clone());
                }
                WorkspaceSecretEvent::ValueUpdated { encrypted_value } => {
                    builder = builder.encrypted_value(encrypted_value.clone());
                }
            }
        }

        builder.events(events).build()
    }
}

#[derive(Debug, Builder)]
pub struct NewWorkspaceSecret {
    #[builder(setter(into))]
    pub(super) id: WorkspaceSecretId,
    #[builder(setter(into))]
    pub(super) workspace_id: WorkspaceId,
    #[builder(setter(into))]
    pub(super) name: String,
    pub(super) secret_type: SecretType,
    pub(super) encrypted_value: Encrypted,
}

impl NewWorkspaceSecret {
    pub fn builder() -> NewWorkspaceSecretBuilder {
        let mut builder = NewWorkspaceSecretBuilder::default();
        builder.id(WorkspaceSecretId::new());
        builder
    }
}

impl IntoEvents<WorkspaceSecretEvent> for NewWorkspaceSecret {
    fn into_events(self) -> EntityEvents<WorkspaceSecretEvent> {
        EntityEvents::init(
            self.id,
            [WorkspaceSecretEvent::Initialized {
                id: self.id,
                workspace_id: self.workspace_id,
                name: self.name,
                secret_type: self.secret_type,
                encrypted_value: self.encrypted_value,
            }],
        )
    }
}
