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

#[cfg(test)]
mod tests {
    use es_entity::{IntoEvents as _, TryFromEvents as _};

    use super::*;
    use crate::encryption::EncryptionKey;

    fn test_key() -> EncryptionKey {
        EncryptionKey::new([42u8; 32])
    }

    fn new_secret(name: &str, secret_type: SecretType, value: &str) -> NewWorkspaceSecret {
        let key = test_key();
        NewWorkspaceSecret::builder()
            .workspace_id(WorkspaceId::new())
            .name(name)
            .secret_type(secret_type)
            .encrypted_value(key.encrypt_string(value))
            .build()
            .expect("build NewWorkspaceSecret")
    }

    fn hydrate(new: NewWorkspaceSecret) -> WorkspaceSecret {
        WorkspaceSecret::try_from_events(new.into_events()).expect("hydrate")
    }

    #[test]
    fn update_value_replaces_encrypted_value() {
        let key = test_key();
        let mut secret = hydrate(new_secret("DB_PASS", SecretType::File, "old-password"));

        let new_encrypted = key.encrypt_string("new-password");
        let _ = secret.update_value(new_encrypted);

        let decrypted = key.decrypt_string(secret.encrypted_value()).unwrap();
        assert_eq!(decrypted, "new-password");
    }

    #[test]
    fn hydrate_with_value_updated_event() {
        let key = test_key();
        let new = new_secret("TOKEN", SecretType::EnvVar, "v1");
        let id = new.id;
        let mut secret = hydrate(new);

        let updated_encrypted = key.encrypt_string("v2");
        let _ = secret.update_value(updated_encrypted);

        // Re-hydrate from the full event stream
        let rehydrated =
            WorkspaceSecret::try_from_events(secret.events.clone()).expect("rehydrate");
        assert_eq!(rehydrated.id, id);
        assert_eq!(rehydrated.name, "TOKEN");
        let decrypted = key.decrypt_string(rehydrated.encrypted_value()).unwrap();
        assert_eq!(decrypted, "v2");
    }

}
