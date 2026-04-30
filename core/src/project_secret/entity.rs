use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::encryption::Encrypted;
use crate::primitives::*;

use super::primitives::SecretType;

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "ProjectSecretId")]
pub enum ProjectSecretEvent {
    Initialized {
        id: ProjectSecretId,
        project_id: ProjectId,
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
pub struct ProjectSecret {
    pub id: ProjectSecretId,
    pub project_id: ProjectId,
    pub name: String,
    pub secret_type: SecretType,
    pub(super) encrypted_value: Encrypted,
    events: EntityEvents<ProjectSecretEvent>,
}

impl ProjectSecret {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }

    pub(super) fn update_value(&mut self, encrypted_value: Encrypted) -> Idempotent<()> {
        self.encrypted_value = encrypted_value.clone();
        self.events
            .push(ProjectSecretEvent::ValueUpdated { encrypted_value });
        Idempotent::Executed(())
    }

    /// Access the encrypted value (for decryption by the service layer).
    pub(super) fn encrypted_value(&self) -> &Encrypted {
        &self.encrypted_value
    }
}

impl core::fmt::Display for ProjectSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ProjectSecret: {}, name: {}, type: {}",
            self.id, self.name, self.secret_type
        )
    }
}

impl TryFromEvents<ProjectSecretEvent> for ProjectSecret {
    fn try_from_events(
        events: EntityEvents<ProjectSecretEvent>,
    ) -> Result<Self, EntityHydrationError> {
        let mut builder = ProjectSecretBuilder::default();

        for event in events.iter_all() {
            match event {
                ProjectSecretEvent::Initialized {
                    id,
                    project_id,
                    name,
                    secret_type,
                    encrypted_value,
                } => {
                    builder = builder
                        .id(*id)
                        .project_id(*project_id)
                        .name(name.clone())
                        .secret_type(*secret_type)
                        .encrypted_value(encrypted_value.clone());
                }
                ProjectSecretEvent::ValueUpdated { encrypted_value } => {
                    builder = builder.encrypted_value(encrypted_value.clone());
                }
            }
        }

        builder.events(events).build()
    }
}

#[derive(Debug, Builder)]
pub struct NewProjectSecret {
    #[builder(setter(into))]
    pub(super) id: ProjectSecretId,
    #[builder(setter(into))]
    pub(super) project_id: ProjectId,
    #[builder(setter(into))]
    pub(super) name: String,
    pub(super) secret_type: SecretType,
    pub(super) encrypted_value: Encrypted,
}

impl NewProjectSecret {
    pub fn builder() -> NewProjectSecretBuilder {
        let mut builder = NewProjectSecretBuilder::default();
        builder.id(ProjectSecretId::new());
        builder
    }
}

impl IntoEvents<ProjectSecretEvent> for NewProjectSecret {
    fn into_events(self) -> EntityEvents<ProjectSecretEvent> {
        EntityEvents::init(
            self.id,
            [ProjectSecretEvent::Initialized {
                id: self.id,
                project_id: self.project_id,
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

    fn new_secret(name: &str, secret_type: SecretType, value: &str) -> NewProjectSecret {
        let key = test_key();
        NewProjectSecret::builder()
            .project_id(ProjectId::new())
            .name(name)
            .secret_type(secret_type)
            .encrypted_value(key.encrypt_string(value))
            .build()
            .expect("build NewProjectSecret")
    }

    fn hydrate(new: NewProjectSecret) -> ProjectSecret {
        ProjectSecret::try_from_events(new.into_events()).expect("hydrate")
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
        let rehydrated = ProjectSecret::try_from_events(secret.events.clone()).expect("rehydrate");
        assert_eq!(rehydrated.id, id);
        assert_eq!(rehydrated.name, "TOKEN");
        let decrypted = key.decrypt_string(rehydrated.encrypted_value()).unwrap();
        assert_eq!(decrypted, "v2");
    }
}
