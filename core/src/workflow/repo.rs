use sqlx::PgPool;

use es_entity::*;

use crate::library::Library;
use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "WorkflowDefinition",
    columns(
        project_id(ty = "ProjectId", list_for(by(created_at))),
        name(ty = "String"),
    ),
    delete = "soft_without_queries",
    post_persist_hook(method = "sync_to_library", error = "crate::library::LibraryError")
)]
pub struct WorkflowDefinitionRepo {
    #[allow(dead_code)]
    pool: PgPool,
    library: Option<Library>,
}

impl WorkflowDefinitionRepo {
    pub fn new(pool: &PgPool, library: Library) -> Self {
        Self {
            pool: pool.clone(),
            library: Some(library),
        }
    }

    pub fn new_without_library(pool: &PgPool) -> Self {
        Self {
            pool: pool.clone(),
            library: None,
        }
    }

    pub async fn cascade_delete_for_project_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        project_id: ProjectId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE workflow_definitions SET deleted = TRUE WHERE project_id = $1")
            .bind(project_id)
            .execute(op.as_executor())
            .await?;
        Ok(())
    }

    async fn sync_to_library<OP: es_entity::AtomicOperation>(
        &self,
        op: &mut OP,
        entity: &WorkflowDefinition,
        mut new_events: es_entity::LastPersisted<'_, WorkflowDefinitionEvent>,
    ) -> Result<(), crate::library::LibraryError> {
        if let Some(library) = &self.library {
            library.sync_entity_in_op(op, entity, &mut new_events).await
        } else {
            Ok(())
        }
    }
}
