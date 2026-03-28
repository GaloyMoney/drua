mod entity;
pub mod error;
pub mod repo;

use tracing::instrument;

use entity::*;
pub use entity::{OutputLine, Task, TaskStatus};
pub use error::*;
use repo::*;

use crate::primitives::*;

#[derive(Clone)]
pub struct Tasks {
    repo: TaskRepo,
}

impl Tasks {
    pub fn new(pool: &sqlx::PgPool) -> Self {
        let repo = TaskRepo::new(pool);
        Self { repo }
    }

    #[instrument(name = "domain.task.create", skip(self))]
    pub async fn create(
        &self,
        description: impl Into<String> + std::fmt::Debug,
    ) -> Result<Task, TaskError> {
        let new_task = NewTask::builder()
            .description(description)
            .build()
            .expect("Could not build new task");

        let task = self.repo.create(new_task).await?;
        Ok(task)
    }

    #[instrument(name = "domain.task.create_in_op", skip(self, op))]
    pub async fn create_in_op(
        &self,
        op: &mut es_entity::DbOp<'_>,
        description: impl Into<String> + std::fmt::Debug,
    ) -> Result<Task, TaskError> {
        let new_task = NewTask::builder()
            .description(description)
            .build()
            .expect("Could not build new task");

        let task = self.repo.create_in_op(op, new_task).await?;
        Ok(task)
    }

    #[instrument(name = "domain.task.find_by_id", skip(self))]
    pub async fn find_by_id(
        &self,
        id: impl Into<TaskId> + std::fmt::Debug,
    ) -> Result<Task, TaskError> {
        Ok(self.repo.find_by_id(id.into()).await?)
    }

    #[instrument(name = "domain.task.list_by_status", skip(self))]
    pub async fn list_by_status(&self, status: TaskStatus) -> Result<Vec<Task>, TaskError> {
        let query = es_entity::PaginatedQueryArgs {
            first: 100,
            after: None,
        };
        let result = self
            .repo
            .list_for_status_by_id(status, query, es_entity::ListDirection::Descending)
            .await?;
        Ok(result.entities)
    }

    #[instrument(name = "domain.task.list_all", skip(self))]
    pub async fn list_all(&self) -> Result<Vec<Task>, TaskError> {
        let mut tasks = Vec::new();
        for status in [
            TaskStatus::Running,
            TaskStatus::Pending,
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            let mut batch = self.list_by_status(status).await?;
            tasks.append(&mut batch);
        }
        Ok(tasks)
    }

    #[instrument(name = "domain.task.start", skip(self))]
    pub async fn start(
        &self,
        id: impl Into<TaskId> + std::fmt::Debug,
        sandbox_name: String,
    ) -> Result<Task, TaskError> {
        let id = id.into();
        let mut task = self.repo.find_by_id(id).await?;

        if task.status != TaskStatus::Pending {
            return Err(TaskError::InvalidStateTransition(format!(
                "Cannot start task in {:?} state",
                task.status
            )));
        }

        task.start(sandbox_name);
        self.repo.update(&mut task).await?;
        Ok(task)
    }

    #[instrument(name = "domain.task.receive_output", skip(self, line))]
    pub async fn receive_output(
        &self,
        id: impl Into<TaskId> + std::fmt::Debug,
        line: String,
    ) -> Result<Task, TaskError> {
        let id = id.into();
        let mut task = self.repo.find_by_id(id).await?;
        task.receive_output(line);
        self.repo.update(&mut task).await?;
        Ok(task)
    }

    #[instrument(name = "domain.task.complete", skip(self))]
    pub async fn complete(
        &self,
        id: impl Into<TaskId> + std::fmt::Debug,
        git_branch: Option<String>,
        pr_url: Option<String>,
    ) -> Result<Task, TaskError> {
        let id = id.into();
        let mut task = self.repo.find_by_id(id).await?;
        task.complete(git_branch, pr_url);
        self.repo.update(&mut task).await?;
        Ok(task)
    }

    #[instrument(name = "domain.task.fail", skip(self))]
    pub async fn fail(
        &self,
        id: impl Into<TaskId> + std::fmt::Debug,
        error_message: String,
    ) -> Result<Task, TaskError> {
        let id = id.into();
        let mut task = self.repo.find_by_id(id).await?;
        task.fail(error_message);
        self.repo.update(&mut task).await?;
        Ok(task)
    }

    #[instrument(name = "domain.task.cancel", skip(self))]
    pub async fn cancel(&self, id: impl Into<TaskId> + std::fmt::Debug) -> Result<Task, TaskError> {
        let id = id.into();
        let mut task = self.repo.find_by_id(id).await?;

        if task.cancel().did_execute() {
            self.repo.update(&mut task).await?;
        }

        Ok(task)
    }
}
