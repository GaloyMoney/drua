mod entity;
pub mod error;
pub(crate) mod repo;

use tracing::instrument;

pub use entity::User;
use entity::*;
pub use error::*;
use repo::*;

use crate::primitives::*;

#[derive(Clone)]
pub struct Users {
    repo: UserRepo,
}

impl Users {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        let repo = UserRepo::new(pool);
        Self { repo }
    }

    #[instrument(name = "mcp_gateway.user.find_by_github_id", skip_all)]
    pub async fn find_by_github_id(&self, github_id: &str) -> Result<Option<User>, UserError> {
        Ok(self.repo.maybe_find_by_github_id(github_id).await?)
    }

    #[instrument(name = "mcp_gateway.user.create_from_github_login", skip_all)]
    pub async fn create_from_github_login(
        &self,
        github_id: impl Into<String> + std::fmt::Debug,
        email: Option<String>,
        name: Option<String>,
    ) -> Result<User, UserError> {
        let new_user = build_new_user(github_id, email, name);
        let user = self.repo.create(new_user).await?;
        Ok(user)
    }

    #[instrument(name = "mcp_gateway.user.create_from_github_login_in_op", skip_all)]
    pub async fn create_from_github_login_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        github_id: impl Into<String> + std::fmt::Debug,
        email: Option<String>,
        name: Option<String>,
    ) -> Result<User, UserError> {
        let new_user = build_new_user(github_id, email, name);
        let user = self.repo.create_in_op(op, new_user).await?;
        Ok(user)
    }

    #[instrument(name = "mcp_gateway.user.find_by_id", skip_all)]
    pub async fn find_by_id(
        &self,
        id: impl Into<UserId> + std::fmt::Debug,
    ) -> Result<User, UserError> {
        Ok(self.repo.find_by_id(id.into()).await?)
    }
}

fn build_new_user(
    github_id: impl Into<String>,
    email: Option<String>,
    name: Option<String>,
) -> NewUser {
    let mut builder = NewUser::builder();
    builder.github_id(github_id);
    if let Some(email) = email {
        builder.email(email);
    }
    if let Some(name) = name {
        builder.name(name);
    }
    builder.build().expect("Could not build new user")
}
