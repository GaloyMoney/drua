pub mod error;
pub mod primitives;

use tracing::instrument;

pub use error::*;
pub use primitives::*;

use crate::primitives::*;

#[derive(Clone)]
pub struct WorkspaceSecrets {
    pool: sqlx::PgPool,
}

impl WorkspaceSecrets {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    /// Create a new secret for a workspace.
    #[instrument(name = "workspace_secret.create", skip_all)]
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        name: &str,
        secret_type: SecretType,
        value: &str,
    ) -> Result<WorkspaceSecret, WorkspaceSecretError> {
        let id = WorkspaceSecretId::new();
        let secret_type_str = secret_type.as_str();

        let row = sqlx::query_as::<_, WorkspaceSecret>(
            r#"INSERT INTO workspace_secrets (id, workspace_id, name, secret_type, encrypted_value)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (workspace_id, name) DO UPDATE
                SET encrypted_value = EXCLUDED.encrypted_value,
                    secret_type = EXCLUDED.secret_type,
                    updated_at = NOW()
            RETURNING id, workspace_id, name, secret_type, created_at, updated_at"#,
        )
        .bind(id)
        .bind(workspace_id)
        .bind(name)
        .bind(secret_type_str)
        .bind(value)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// List secrets for a workspace (metadata only, no values).
    #[instrument(name = "workspace_secret.list_by_workspace", skip_all)]
    pub async fn list_by_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<WorkspaceSecret>, WorkspaceSecretError> {
        let rows = sqlx::query_as::<_, WorkspaceSecret>(
            r#"SELECT id, workspace_id, name, secret_type, created_at, updated_at
            FROM workspace_secrets
            WHERE workspace_id = $1
            ORDER BY name ASC"#,
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Find a secret by ID (metadata only).
    #[instrument(name = "workspace_secret.find_by_id", skip_all)]
    pub async fn find_by_id(
        &self,
        id: WorkspaceSecretId,
    ) -> Result<WorkspaceSecret, WorkspaceSecretError> {
        sqlx::query_as::<_, WorkspaceSecret>(
            r#"SELECT id, workspace_id, name, secret_type, created_at, updated_at
            FROM workspace_secrets
            WHERE id = $1"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(WorkspaceSecretError::SecretNotFound)
    }

    /// Update a secret's value.
    #[instrument(name = "workspace_secret.update_value", skip_all)]
    pub async fn update_value(
        &self,
        id: WorkspaceSecretId,
        value: &str,
    ) -> Result<WorkspaceSecret, WorkspaceSecretError> {
        let row = sqlx::query_as::<_, WorkspaceSecret>(
            r#"UPDATE workspace_secrets
            SET encrypted_value = $1, updated_at = NOW()
            WHERE id = $2
            RETURNING id, workspace_id, name, secret_type, created_at, updated_at"#,
        )
        .bind(value)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(WorkspaceSecretError::SecretNotFound)?;
        Ok(row)
    }

    /// Delete a secret.
    #[instrument(name = "workspace_secret.delete", skip_all)]
    pub async fn delete(&self, id: WorkspaceSecretId) -> Result<(), WorkspaceSecretError> {
        let result = sqlx::query(r#"DELETE FROM workspace_secrets WHERE id = $1"#)
            .bind(id)
            .execute(&self.pool)
            .await?;

        if result.rows_affected() == 0 {
            return Err(WorkspaceSecretError::SecretNotFound);
        }
        Ok(())
    }

    /// List secrets with values for a workspace (internal use — harness injection).
    #[instrument(name = "workspace_secret.list_with_values", skip_all)]
    pub async fn list_with_values(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<WorkspaceSecretWithValue>, WorkspaceSecretError> {
        let rows = sqlx::query_as::<_, WorkspaceSecretWithValue>(
            r#"SELECT id, workspace_id, name, secret_type, encrypted_value, created_at, updated_at
            FROM workspace_secrets
            WHERE workspace_id = $1
            ORDER BY name ASC"#,
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}
