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
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
        name: &str,
        secret_type: SecretType,
        value: &str,
    ) -> Result<WorkspaceSecret, WorkspaceSecretError> {
        sub.can(
            AuthVerb::Create,
            AuthResource::WorkspaceSecret(workspace_id, None),
        )?;
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
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<WorkspaceSecret>, WorkspaceSecretError> {
        sub.can(
            AuthVerb::Read,
            AuthResource::WorkspaceSecret(workspace_id, None),
        )?;
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
        sub: &AuthSubject,
        id: WorkspaceSecretId,
    ) -> Result<WorkspaceSecret, WorkspaceSecretError> {
        let secret = self.repo.find_by_id(id).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::WorkspaceSecret(secret.workspace_id, Some(secret.id)),
        )?;
        Ok(secret)
    }

    /// Update a secret's value.
    #[instrument(name = "workspace_secret.update_value", skip_all)]
    pub async fn update_value(
        &self,
        sub: &AuthSubject,
        id: WorkspaceSecretId,
        value: &str,
    ) -> Result<WorkspaceSecret, WorkspaceSecretError> {
        let mut op = self.repo.begin_op().await?;
        let mut secret = self.repo.find_by_id_in_op(&mut op, id).await?;
        sub.can(
            AuthVerb::Update,
            AuthResource::WorkspaceSecret(secret.workspace_id, Some(secret.id)),
        )?;
        let encrypted_value = self.encryption_key.encrypt_string(value);
        if secret.update_value(encrypted_value).did_execute() {
            self.repo.update_in_op(&mut op, &mut secret).await?;
        }
        op.commit().await?;
        Ok(secret)
    }

    /// Delete a secret (soft delete via archive).
    #[instrument(name = "workspace_secret.delete", skip_all)]
    pub async fn delete(
        &self,
        sub: &AuthSubject,
        id: WorkspaceSecretId,
    ) -> Result<(), WorkspaceSecretError> {
        let mut op = self.repo.begin_op().await?;
        let secret = self.repo.find_by_id_in_op(&mut op, id).await?;
        sub.can(
            AuthVerb::Delete,
            AuthResource::WorkspaceSecret(secret.workspace_id, Some(secret.id)),
        )?;
        self.repo.delete_in_op(&mut op, secret).await?;
        op.commit().await?;
        Ok(())
    }

    /// Bulk soft-delete all secrets belonging to a workspace within a
    /// transaction. Used during workspace cascade deletion.
    #[instrument(name = "workspace_secret.delete_for_workspace_in_op", skip_all)]
    pub(crate) async fn delete_for_workspace_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        workspace_id: WorkspaceId,
    ) -> Result<(), WorkspaceSecretError> {
        self.repo
            .cascade_delete_for_workspace_in_op(op, workspace_id)
            .await?;
        Ok(())
    }

    /// List secrets with decrypted values for a workspace (internal use — harness injection).
    #[instrument(name = "workspace_secret.list_decrypted", skip_all)]
    pub async fn list_decrypted(
        &self,
        sub: &AuthSubject,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<DecryptedSecret>, WorkspaceSecretError> {
        sub.can(
            AuthVerb::Read,
            AuthResource::WorkspaceSecret(workspace_id, None),
        )?;
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
