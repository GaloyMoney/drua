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

    /// Whether a row with this id exists with `deleted = TRUE`.
    /// `soft_without_queries` doesn't auto-generate `_include_deleted`
    /// query variants, so reverse-sync needs a direct probe to avoid
    /// resurrecting an entity an operator just removed.
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

    /// Resolve a canonical-path string back to the live workflow row.
    /// Reverse-sync uses this when an external git delete removes the
    /// YAML — the importer has only the path string to identify the
    /// entity. Matches on `(project_id, canonical_workflow_path(name,
    /// project_name))` exactly so a slug collision in two different
    /// projects can't cross-link.
    pub async fn maybe_find_by_canonical_path_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        project_id: ProjectId,
        path: &str,
    ) -> Result<Option<WorkflowDefinition>, super::WorkflowError> {
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .list_for_project_id_by_created_at_in_op(
                op,
                project_id,
                query,
                es_entity::ListDirection::Ascending,
            )
            .await?;
        Ok(result
            .entities
            .into_iter()
            .find(|w| canonical_workflow_path(&w.name, w.project_name.as_deref()) == path))
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
