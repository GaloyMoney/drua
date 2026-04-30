mod entity;
pub mod error;
pub mod primitives;
pub(crate) mod repo;

use tracing::instrument;

pub use crate::primitives::*;
pub use entity::ProjectSecret;
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
pub struct ProjectSecrets {
    repo: ProjectSecretRepo,
    encryption_key: EncryptionKey,
}

impl ProjectSecrets {
    pub fn new(pool: &sqlx::PgPool, encryption_key: EncryptionKey) -> Self {
        let repo = ProjectSecretRepo::new(pool);
        Self {
            repo,
            encryption_key,
        }
    }

    /// Create a new secret. Fails if a secret with the same name already exists in the project.
    #[instrument(name = "project_secret.create", skip_all)]
    pub async fn create(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
        name: &str,
        secret_type: SecretType,
        value: &str,
    ) -> Result<ProjectSecret, ProjectSecretError> {
        sub.can(
            AuthVerb::Create,
            AuthResource::ProjectSecret(project_id, None),
        )?;
        let encrypted_value = self.encryption_key.encrypt_string(value);

        let new = NewProjectSecret::builder()
            .project_id(project_id)
            .name(name)
            .secret_type(secret_type)
            .encrypted_value(encrypted_value)
            .build()
            .expect("Could not build new project secret");

        let secret = self.repo.create(new).await?;
        Ok(secret)
    }

    /// List secrets for a project (metadata only, no decrypted values).
    #[instrument(name = "project_secret.list_by_project", skip_all)]
    pub async fn list_by_project(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
    ) -> Result<Vec<ProjectSecret>, ProjectSecretError> {
        sub.can(
            AuthVerb::Read,
            AuthResource::ProjectSecret(project_id, None),
        )?;
        self.list_all_for_project(project_id).await
    }

    async fn list_all_for_project(
        &self,
        project_id: ProjectId,
    ) -> Result<Vec<ProjectSecret>, ProjectSecretError> {
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_project_id_by_created_at(
                project_id,
                query,
                es_entity::ListDirection::Descending,
            )
            .await?;
        Ok(result.entities)
    }

    #[instrument(name = "project_secret.find_by_id", skip_all)]
    pub async fn find_by_id(
        &self,
        sub: &AuthSubject,
        id: ProjectSecretId,
    ) -> Result<ProjectSecret, ProjectSecretError> {
        let secret = self.repo.find_by_id(id).await?;
        sub.can(
            AuthVerb::Read,
            AuthResource::ProjectSecret(secret.project_id, Some(secret.id)),
        )?;
        Ok(secret)
    }

    #[instrument(name = "project_secret.update_value", skip_all)]
    pub async fn update_value(
        &self,
        sub: &AuthSubject,
        id: ProjectSecretId,
        value: &str,
    ) -> Result<ProjectSecret, ProjectSecretError> {
        let mut op = self.repo.begin_op().await?;
        let mut secret = self.repo.find_by_id_in_op(&mut op, id).await?;
        sub.can(
            AuthVerb::Update,
            AuthResource::ProjectSecret(secret.project_id, Some(secret.id)),
        )?;
        let encrypted_value = self.encryption_key.encrypt_string(value);
        if secret.update_value(encrypted_value).did_execute() {
            self.repo.update_in_op(&mut op, &mut secret).await?;
        }
        op.commit().await?;
        Ok(secret)
    }

    /// Delete a secret (soft delete via archive).
    #[instrument(name = "project_secret.delete", skip_all)]
    pub async fn delete(
        &self,
        sub: &AuthSubject,
        id: ProjectSecretId,
    ) -> Result<(), ProjectSecretError> {
        let mut op = self.repo.begin_op().await?;
        let secret = self.repo.find_by_id_in_op(&mut op, id).await?;
        sub.can(
            AuthVerb::Delete,
            AuthResource::ProjectSecret(secret.project_id, Some(secret.id)),
        )?;
        self.repo.delete_in_op(&mut op, secret).await?;
        op.commit().await?;
        Ok(())
    }

    /// Bulk soft-delete all secrets belonging to a project within a
    /// transaction. Used during project cascade deletion.
    #[instrument(name = "project_secret.delete_for_project_in_op", skip_all)]
    pub(crate) async fn delete_for_project_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        project_id: ProjectId,
    ) -> Result<(), ProjectSecretError> {
        self.repo
            .cascade_delete_for_project_in_op(op, project_id)
            .await?;
        Ok(())
    }

    /// List secrets with decrypted values for a project (internal use — harness injection).
    #[instrument(name = "project_secret.list_decrypted", skip_all)]
    pub async fn list_decrypted(
        &self,
        sub: &AuthSubject,
        project_id: ProjectId,
    ) -> Result<Vec<DecryptedSecret>, ProjectSecretError> {
        sub.can(
            AuthVerb::Read,
            AuthResource::ProjectSecret(project_id, None),
        )?;
        let secrets = self.list_all_for_project(project_id).await?;

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
