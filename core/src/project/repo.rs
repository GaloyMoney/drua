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
            // `into_next_query` needs `entities.len()` intact, so read
            // pagination fields before draining `entities` below.
            let has_next_page = result.has_next_page;
            let next_first = result.entities.len();
            let next_after = result.end_cursor.take();
            all.extend(
                result
                    .entities
                    .drain(..)
                    .filter(|project| !project.is_archived()),
            );
            if !has_next_page {
                break;
            }
            query = PaginatedQueryArgs {
                first: next_first,
                after: next_after,
            };
        }

        Ok(all)
    }
}
