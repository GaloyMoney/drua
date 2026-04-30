use sqlx::PgPool;

use es_entity::*;

use crate::primitives::*;

use super::entity::*;

#[derive(EsRepo, Clone)]
#[es_repo(
    entity = "Project",
    columns(name(ty = "String", list_by)),
    delete = "soft_without_queries"
)]
pub struct ProjectRepo {
    #[allow(dead_code)]
    pool: PgPool,
}

impl ProjectRepo {
    pub fn new(pool: &PgPool) -> Self {
        Self { pool: pool.clone() }
    }

    pub async fn list_all(&self) -> Result<Vec<Project>, ProjectQueryError> {
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
            all.extend(
                result
                    .entities
                    .drain(..)
                    .filter(|project| !project.is_archived()),
            );
            match result.into_next_query() {
                Some(next) => query = next,
                None => break,
            }
        }

        Ok(all)
    }
}
