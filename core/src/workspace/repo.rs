use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "Workspace",
    columns(name(ty = "String", list_by)),
    delete = "soft_without_queries"
)]
pub struct WorkspaceRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl WorkspaceRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    pub async fn list_all(&self) -> Result<Vec<Workspace>, WorkspaceQueryError> {
        const PAGE_SIZE: usize = 100;
        let mut all = Vec::new();
        let mut query = PaginatedQueryArgs {
            first: PAGE_SIZE,
            after: None,
        };

        loop {
            let mut result = self
                .list_by_created_at(query, ListDirection::Descending)
                .await?;
            all.extend(result.entities.drain(..).filter(|ws| !ws.is_archived()));
            match result.into_next_query() {
                Some(next) => query = next,
                None => break,
            }
        }

        Ok(all)
    }
}
