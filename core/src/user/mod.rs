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

    #[instrument(name = "domain.user.find_by_github_id", skip(self))]
    pub async fn find_by_github_id(&self, github_id: &str) -> Result<Option<User>, UserError> {
        Ok(self.repo.maybe_find_by_github_id(github_id).await?)
    }

    #[instrument(name = "domain.user.create_from_github_login", skip(self))]
    pub async fn create_from_github_login(
        &self,
        github_id: impl Into<String> + std::fmt::Debug,
        email: Option<String>,
        name: Option<String>,
        github_username: Option<String>,
    ) -> Result<User, UserError> {
        let new_user = build_new_user(github_id, email, name, github_username);
        let user = self.repo.create(new_user).await?;
        Ok(user)
    }

    #[instrument(name = "domain.user.create_from_github_login_in_op", skip(self, op))]
    pub async fn create_from_github_login_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        github_id: impl Into<String> + std::fmt::Debug,
        email: Option<String>,
        name: Option<String>,
        github_username: Option<String>,
    ) -> Result<User, UserError> {
        let new_user = build_new_user(github_id, email, name, github_username);
        let user = self.repo.create_in_op(op, new_user).await?;
        Ok(user)
    }

    #[instrument(name = "domain.user.find_by_id", skip(self))]
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
    github_username: Option<String>,
) -> NewUser {
    let mut builder = NewUser::builder();
    builder.github_id(github_id);
    if let Some(email) = email {
        builder.email(email);
    }
    if let Some(name) = name {
        builder.name(name);
    }
    if let Some(github_username) = github_username {
        builder.github_username(github_username);
    }
    builder.build().expect("Could not build new user")
}
