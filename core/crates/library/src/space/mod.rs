mod entity;
pub mod error;
pub(crate) mod repo;

use std::sync::Arc;

pub use entity::{NewSpace, Space, SpaceEvent};
pub use error::*;

use self::repo::SpaceRepo;
use crate::git::GitEngine;

#[derive(Clone)]
pub struct Spaces {
    git: Arc<GitEngine>,
    repo: SpaceRepo,
}

impl Spaces {
    pub fn new(git: &Arc<GitEngine>, pool: &sqlx::PgPool) -> Self {
        Self {
            git: Arc::clone(git),
            repo: SpaceRepo::new(pool),
        }
    }

    #[tracing::instrument(name = "library.spaces.create_in_op", skip_all, fields(%slug))]
    pub async fn create_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        slug: String,
        description: Option<String>,
    ) -> Result<Space, SpaceError> {
        let mut builder = NewSpace::builder().slug(slug);
        if let Some(desc) = description {
            builder = builder.description(desc);
        }
        let new_space = builder.build()?;
        let space = self.repo.create_in_op(op, new_space).await?;

        self.git
            .update_file(
                format!("spaces/{}/.gitkeep", space.slug),
                |_input_path, _current| (None, Some(Vec::new())),
                format!("space: init {}", space.slug),
            )
            .await
            .map_err(|e| SpaceError::Git(e.to_string()))?;

        Ok(space)
    }
}
