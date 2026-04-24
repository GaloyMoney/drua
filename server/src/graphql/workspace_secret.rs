use std::sync::Arc;

use async_graphql::{ComplexObject, Enum, InputObject, SimpleObject};

use super::primitives::*;

use drua_core::workspace_secret::primitives::SecretType as DomainSecretType;
use drua_core::workspace_secret::WorkspaceSecret as DomainWorkspaceSecret;

#[derive(SimpleObject, Clone)]
#[graphql(complex)]
pub struct WorkspaceSecret {
    id: WorkspaceSecretId,
    workspace_id: WorkspaceId,
    name: String,
    secret_type: SecretTypeEnum,

    #[graphql(skip)]
    entity: Arc<DomainWorkspaceSecret>,
}

#[ComplexObject]
impl WorkspaceSecret {
    async fn created_at(&self) -> Timestamp {
        self.entity.created_at().into()
    }
}

impl From<DomainWorkspaceSecret> for WorkspaceSecret {
    fn from(entity: DomainWorkspaceSecret) -> Self {
        Self {
            id: entity.id,
            workspace_id: entity.workspace_id,
            name: entity.name.clone(),
            secret_type: SecretTypeEnum::from(entity.secret_type),
            entity: Arc::new(entity),
        }
    }
}

#[derive(Enum, Clone, Copy, PartialEq, Eq)]
pub enum SecretTypeEnum {
    EnvVar,
    File,
}

impl From<DomainSecretType> for SecretTypeEnum {
    fn from(t: DomainSecretType) -> Self {
        match t {
            DomainSecretType::EnvVar => Self::EnvVar,
            DomainSecretType::File => Self::File,
        }
    }
}

impl From<SecretTypeEnum> for DomainSecretType {
    fn from(t: SecretTypeEnum) -> Self {
        match t {
            SecretTypeEnum::EnvVar => Self::EnvVar,
            SecretTypeEnum::File => Self::File,
        }
    }
}

#[derive(InputObject)]
pub struct WorkspaceSecretCreateInput {
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub secret_type: SecretTypeEnum,
    pub value: String,
}

mutation_payload! { WorkspaceSecretCreatePayload, workspace_secret: WorkspaceSecret }

#[derive(InputObject)]
pub struct WorkspaceSecretDeleteInput {
    pub id: WorkspaceSecretId,
}

#[derive(async_graphql::SimpleObject)]
pub struct WorkspaceSecretDeletePayload {
    pub deleted_id: WorkspaceSecretId,
}
