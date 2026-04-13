mod entity;
pub mod error;
pub mod primitives;
pub(crate) mod repo;

use tracing::instrument;

pub use crate::primitives::*;
pub use entity::WorkspaceSecret;
use entity::*;
pub use error::*;
pub use primitives::*;
use repo::*;

use crate::encryption::EncryptionKey;

/// A decrypted secret ready for harness injection.
pub struct DecryptedSecret {
    pub name: String,
    pub secret_type: SecretType,
    pub value: String,
}

#[derive(Clone)]
pub struct WorkspaceSecrets {
    repo: WorkspaceSecretRepo,
    encryption_key: EncryptionKey,
}

impl WorkspaceSecrets {
    pub fn new(pool: &sqlx::PgPool, encryption_key: EncryptionKey) -> Self {
        let repo = WorkspaceSecretRepo::new(pool);
        Self {
            repo,
            encryption_key,
        }
    }

    /// Create a new secret. Fails if a secret with the same name already exists in the workspace.
    #[instrument(name = "workspace_secret.create", skip_all)]
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        name: &str,
        secret_type: SecretType,
        value: &str,
    ) -> Result<WorkspaceSecret, WorkspaceSecretError> {
        let encrypted_value = self.encryption_key.encrypt_string(value);

        let new = NewWorkspaceSecret::builder()
            .workspace_id(workspace_id)
            .name(name)
            .secret_type(secret_type)
            .encrypted_value(encrypted_value)
            .build()
            .expect("Could not build new workspace secret");

        let secret = self.repo.create(new).await?;
        Ok(secret)
    }

    /// List secrets for a workspace (metadata only, no decrypted values).
    #[instrument(name = "workspace_secret.list_by_workspace", skip_all)]
    pub async fn list_by_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<WorkspaceSecret>, WorkspaceSecretError> {
        self.list_all_for_workspace(workspace_id).await
    }

    async fn list_all_for_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<WorkspaceSecret>, WorkspaceSecretError> {
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_workspace_id_by_created_at(
                workspace_id,
                query,
                es_entity::ListDirection::Descending,
            )
            .await?;
        Ok(result.entities)
    }

    /// Find a secret by ID.
    #[instrument(name = "workspace_secret.find_by_id", skip_all)]
    pub async fn find_by_id(
        &self,
        id: WorkspaceSecretId,
    ) -> Result<WorkspaceSecret, WorkspaceSecretError> {
        Ok(self.repo.find_by_id(id).await?)
    }

    /// Update a secret's value.
    #[instrument(name = "workspace_secret.update_value", skip_all)]
    pub async fn update_value(
        &self,
        id: WorkspaceSecretId,
        value: &str,
    ) -> Result<WorkspaceSecret, WorkspaceSecretError> {
        let mut secret = self.repo.find_by_id(id).await?;
        let encrypted_value = self.encryption_key.encrypt_string(value);
        if secret.update_value(encrypted_value).did_execute() {
            self.repo.update(&mut secret).await?;
        }
        Ok(secret)
    }

    /// Delete a secret (soft delete via archive).
    #[instrument(name = "workspace_secret.delete", skip_all)]
    pub async fn delete(&self, id: WorkspaceSecretId) -> Result<(), WorkspaceSecretError> {
        let secret = self.repo.find_by_id(id).await?;
        self.repo.delete(secret).await?;
        Ok(())
    }

    /// List secrets with decrypted values for a workspace (internal use — harness injection).
    #[instrument(name = "workspace_secret.list_decrypted", skip_all)]
    pub async fn list_decrypted(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<DecryptedSecret>, WorkspaceSecretError> {
        let secrets = self.list_all_for_workspace(workspace_id).await?;

        let mut result = Vec::with_capacity(secrets.len());
        for secret in secrets {
            let value = self
                .encryption_key
                .decrypt_string(secret.encrypted_value())?;
            result.push(DecryptedSecret {
                name: secret.name.clone(),
                secret_type: secret.secret_type,
                value,
            });
        }
        Ok(result)
    }
}
