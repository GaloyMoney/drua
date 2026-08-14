use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;
use super::yaml::canonical_workflow_path;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "WorkflowDefinition",
    columns(
        project_id(ty = "ProjectId", list_for(by(created_at))),
        name(ty = "String"),
        provider(
            ty = "Option<String>",
            list_for(by(created_at)),
            create(accessor = "provider()"),
            update(accessor = "provider()")
        ),
    ),
    delete = "soft_without_queries",
    post_persist_hook(method = "sync_to_library", error = "drua_library::LibraryError")
)]
pub struct WorkflowDefinitionRepo {
    pool: PgPool,
    library: Option<drua_library::Library>,
}

impl WorkflowDefinitionRepo {
    pub fn new(pool: &PgPool, library: drua_library::Library) -> Self {
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

    pub async fn is_soft_deleted_in_op(
        &self,
        op: &mut impl AtomicOperation,
        id: WorkflowDefinitionId,
    ) -> Result<bool, sqlx::Error> {
        let row: Option<(bool,)> =
            sqlx::query_as("SELECT deleted FROM workflow_definitions WHERE id = $1")
                .bind(id)
                .fetch_optional(op.as_executor())
                .await?;
        Ok(row.map(|(d,)| d).unwrap_or(false))
    }

    /// Standalone liveness read (no op): `true` when the row is present and
    /// not soft-deleted. A missing row reads as not-live. Used by the library
    /// forward-sync write job's liveness check.
    pub async fn is_live(&self, id: WorkflowDefinitionId) -> Result<bool, sqlx::Error> {
        let row: Option<(bool,)> =
            sqlx::query_as("SELECT deleted FROM workflow_definitions WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(matches!(row, Some((false,))))
    }

    pub async fn maybe_find_by_canonical_path_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        project_id: ProjectId,
        path: &str,
    ) -> Result<Option<WorkflowDefinition>, super::WorkflowError> {
        let mut query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        loop {
            let mut result = self
                .list_for_project_id_by_created_at_in_op(
                    &mut *op,
                    project_id,
                    query,
                    es_entity::ListDirection::Ascending,
                )
                .await?;
            // `into_next_query` needs `entities.len()` intact, so read
            // pagination fields before taking `entities` below.
            let has_next_page = result.has_next_page;
            let next_first = result.entities.len();
            let next_after = result.end_cursor.take();
            let entities = std::mem::take(&mut result.entities);
            if let Some(found) = entities
                .into_iter()
                .find(|w| canonical_workflow_path(&w.name, w.project_name.as_deref()) == path)
            {
                return Ok(Some(found));
            }
            if !has_next_page {
                return Ok(None);
            }
            query = es_entity::PaginatedQueryArgs {
                first: next_first,
                after: next_after,
            };
        }
    }

    async fn sync_to_library<OP: es_entity::AtomicOperation>(
        &self,
        op: &mut OP,
        entity: &WorkflowDefinition,
        mut new_events: es_entity::LastPersisted<'_, WorkflowDefinitionEvent>,
    ) -> Result<(), drua_library::LibraryError> {
        if let Some(library) = &self.library {
            library.sync_entity_in_op(op, entity, &mut new_events).await
        } else {
            Ok(())
        }
    }
}
