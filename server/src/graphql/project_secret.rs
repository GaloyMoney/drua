use std::sync::Arc;

use async_graphql::{ComplexObject, Enum, InputObject, SimpleObject};

use super::primitives::*;

use drua_core::project_secret::primitives::SecretType as DomainSecretType;
use drua_core::project_secret::ProjectSecret as DomainProjectSecret;

#[derive(SimpleObject, Clone)]
#[graphql(complex)]
pub struct ProjectSecret {
    id: ProjectSecretId,
    project_id: ProjectId,
    name: String,
    secret_type: SecretTypeEnum,

    #[graphql(skip)]
    entity: Arc<DomainProjectSecret>,
}

#[ComplexObject]
impl ProjectSecret {
    async fn created_at(&self) -> Timestamp {
        self.entity.created_at().into()
    }
}

impl From<DomainProjectSecret> for ProjectSecret {
    fn from(entity: DomainProjectSecret) -> Self {
        Self {
            id: entity.id,
            project_id: entity.project_id,
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
pub struct ProjectSecretCreateInput {
    pub project_id: ProjectId,
    pub name: String,
    pub secret_type: SecretTypeEnum,
    pub value: String,
}

mutation_payload! { ProjectSecretCreatePayload, project_secret: ProjectSecret }

#[derive(InputObject)]
pub struct ProjectSecretDeleteInput {
    pub id: ProjectSecretId,
}

#[derive(async_graphql::SimpleObject)]
pub struct ProjectSecretDeletePayload {
    pub deleted_id: ProjectSecretId,
}
