use derive_builder::Builder;
use serde::{Deserialize, Serialize};

use es_entity::*;

use crate::primitives::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Running => "running",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
        }
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for TaskStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(TaskStatus::Pending),
            "running" => Ok(TaskStatus::Running),
            "completed" => Ok(TaskStatus::Completed),
            "failed" => Ok(TaskStatus::Failed),
            "cancelled" => Ok(TaskStatus::Cancelled),
            other => Err(format!("Unknown task status: {other}")),
        }
    }
}

impl sqlx::Type<sqlx::Postgres> for TaskStatus {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <String as sqlx::Type<sqlx::Postgres>>::type_info()
    }
}

impl<'q> sqlx::Encode<'q, sqlx::Postgres> for TaskStatus {
    fn encode_by_ref(
        &self,
        buf: &mut sqlx::postgres::PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, Box<dyn std::error::Error + Send + Sync>> {
        <&str as sqlx::Encode<'q, sqlx::Postgres>>::encode_by_ref(&self.as_str(), buf)
    }
}

impl sqlx::Decode<'_, sqlx::Postgres> for TaskStatus {
    fn decode(
        value: sqlx::postgres::PgValueRef<'_>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let s = <String as sqlx::Decode<'_, sqlx::Postgres>>::decode(value)?;
        s.parse::<TaskStatus>()
            .map_err(|e| Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)) as _)
    }
}

impl sqlx::postgres::PgHasArrayType for TaskStatus {
    fn array_type_info() -> sqlx::postgres::PgTypeInfo {
        <String as sqlx::postgres::PgHasArrayType>::array_type_info()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputLine {
    pub line: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(EsEvent, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[es_event(id = "TaskId")]
pub enum TaskEvent {
    Initialized {
        id: TaskId,
        description: String,
    },
    Started {
        sandbox_name: String,
        started_at: chrono::DateTime<chrono::Utc>,
    },
    OutputReceived {
        line: String,
        timestamp: chrono::DateTime<chrono::Utc>,
    },
    Completed {
        git_branch: Option<String>,
        pr_url: Option<String>,
        completed_at: chrono::DateTime<chrono::Utc>,
    },
    Failed {
        error_message: String,
        failed_at: chrono::DateTime<chrono::Utc>,
    },
    Cancelled {
        cancelled_at: chrono::DateTime<chrono::Utc>,
    },
}

#[derive(EsEntity, Builder)]
#[builder(pattern = "owned", build_fn(error = "EntityHydrationError"))]
pub struct Task {
    pub id: TaskId,
    pub description: String,
    pub status: TaskStatus,
    #[builder(setter(strip_option), default)]
    pub sandbox_name: Option<String>,
    #[builder(default)]
    pub output: Vec<OutputLine>,
    #[builder(setter(strip_option), default)]
    pub git_branch: Option<String>,
    #[builder(setter(strip_option), default)]
    pub pr_url: Option<String>,
    #[builder(setter(strip_option), default)]
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    #[builder(setter(strip_option), default)]
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    #[builder(setter(strip_option), default)]
    pub error_message: Option<String>,
    events: EntityEvents<TaskEvent>,
}

impl Task {
    pub fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.events
            .entity_first_persisted_at()
            .expect("entity_first_persisted_at not found")
    }

    pub(super) fn start(&mut self, sandbox_name: String) {
        let started_at = chrono::Utc::now();
        self.events.push(TaskEvent::Started {
            sandbox_name: sandbox_name.clone(),
            started_at,
        });
        self.sandbox_name = Some(sandbox_name);
        self.started_at = Some(started_at);
        self.status = TaskStatus::Running;
    }

    pub(super) fn receive_output(&mut self, line: String) {
        let timestamp = chrono::Utc::now();
        self.events.push(TaskEvent::OutputReceived {
            line: line.clone(),
            timestamp,
        });
        self.output.push(OutputLine { line, timestamp });
    }

    pub(super) fn complete(&mut self, git_branch: Option<String>, pr_url: Option<String>) {
        let completed_at = chrono::Utc::now();
        self.events.push(TaskEvent::Completed {
            git_branch: git_branch.clone(),
            pr_url: pr_url.clone(),
            completed_at,
        });
        self.git_branch = git_branch;
        self.pr_url = pr_url;
        self.completed_at = Some(completed_at);
        self.status = TaskStatus::Completed;
    }

    pub(super) fn fail(&mut self, error_message: String) {
        let failed_at = chrono::Utc::now();
        self.events.push(TaskEvent::Failed {
            error_message: error_message.clone(),
            failed_at,
        });
        self.error_message = Some(error_message);
        self.completed_at = Some(failed_at);
        self.status = TaskStatus::Failed;
    }

    pub(super) fn cancel(&mut self) -> Idempotent<()> {
        idempotency_guard!(
            self.events.iter_all().rev(),
            already_applied: TaskEvent::Cancelled { .. }
        );

        let cancelled_at = chrono::Utc::now();
        self.events.push(TaskEvent::Cancelled { cancelled_at });
        self.completed_at = Some(cancelled_at);
        self.status = TaskStatus::Cancelled;

        Idempotent::Executed(())
    }
}

impl core::fmt::Display for Task {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Task: {}, status: {:?}", self.id, self.status)
    }
}

impl TryFromEvents<TaskEvent> for Task {
    fn try_from_events(events: EntityEvents<TaskEvent>) -> Result<Self, EntityHydrationError> {
        let mut builder = TaskBuilder::default();
        builder = builder.output(Vec::new());

        for event in events.iter_all() {
            match event {
                TaskEvent::Initialized { id, description } => {
                    builder = builder
                        .id(*id)
                        .description(description.clone())
                        .status(TaskStatus::Pending);
                }
                TaskEvent::Started {
                    sandbox_name,
                    started_at,
                } => {
                    builder = builder
                        .sandbox_name(sandbox_name.clone())
                        .started_at(*started_at)
                        .status(TaskStatus::Running);
                }
                TaskEvent::OutputReceived { line, timestamp } => {
                    // Builder doesn't support push; we handle after build
                    // by re-building output from events. Instead, accumulate
                    // via a temporary approach: we'll collect after.
                    // Actually, builder has .output() setter, so we need to
                    // collect lines in a vec and set at end.
                    // For simplicity we do it below.
                    let _ = (line, timestamp);
                }
                TaskEvent::Completed {
                    git_branch,
                    pr_url,
                    completed_at,
                } => {
                    builder = builder
                        .completed_at(*completed_at)
                        .status(TaskStatus::Completed);
                    if let Some(branch) = git_branch {
                        builder = builder.git_branch(branch.clone());
                    }
                    if let Some(url) = pr_url {
                        builder = builder.pr_url(url.clone());
                    }
                }
                TaskEvent::Failed {
                    error_message,
                    failed_at,
                } => {
                    builder = builder
                        .error_message(error_message.clone())
                        .completed_at(*failed_at)
                        .status(TaskStatus::Failed);
                }
                TaskEvent::Cancelled { cancelled_at } => {
                    builder = builder
                        .completed_at(*cancelled_at)
                        .status(TaskStatus::Cancelled);
                }
            }
        }

        // Rebuild output from events
        let output: Vec<OutputLine> = events
            .iter_all()
            .filter_map(|e| match e {
                TaskEvent::OutputReceived { line, timestamp } => Some(OutputLine {
                    line: line.clone(),
                    timestamp: *timestamp,
                }),
                _ => None,
            })
            .collect();
        builder = builder.output(output);

        builder.events(events).build()
    }
}

#[derive(Debug, Builder)]
pub struct NewTask {
    #[builder(setter(into))]
    pub(super) id: TaskId,
    #[builder(setter(into))]
    pub(super) description: String,
    pub(super) status: TaskStatus,
}

impl NewTask {
    pub fn builder() -> NewTaskBuilder {
        let mut builder = NewTaskBuilder::default();
        builder.id(TaskId::new());
        builder.status(TaskStatus::Pending);
        builder
    }
}

impl IntoEvents<TaskEvent> for NewTask {
    fn into_events(self) -> EntityEvents<TaskEvent> {
        EntityEvents::init(
            self.id,
            [TaskEvent::Initialized {
                id: self.id,
                description: self.description,
            }],
        )
    }
}

#[cfg(test)]
mod tests {
    use es_entity::{Idempotent, IntoEvents as _, TryFromEvents as _};

    use crate::primitives::TaskId;

    use super::{NewTask, Task, TaskStatus};

    fn new_task() -> Task {
        let new_task = NewTask::builder()
            .id(TaskId::new())
            .description("Test task description")
            .build()
            .unwrap();

        Task::try_from_events(new_task.into_events()).unwrap()
    }

    #[test]
    fn task_hydration() {
        let task = new_task();
        assert_eq!(task.description, "Test task description");
        assert_eq!(task.status, TaskStatus::Pending);
        assert!(task.sandbox_name.is_none());
        assert!(task.output.is_empty());
    }

    #[test]
    fn task_lifecycle_success() {
        let mut task = new_task();

        task.start("sandbox-abc".to_string());
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(task.sandbox_name.as_deref(), Some("sandbox-abc"));

        task.receive_output("Starting work...".to_string());
        assert_eq!(task.output.len(), 1);

        task.complete(
            Some("feature/test".to_string()),
            Some("https://github.com/org/repo/pull/1".to_string()),
        );
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.git_branch.as_deref(), Some("feature/test"));
    }

    #[test]
    fn task_lifecycle_failure() {
        let mut task = new_task();

        task.start("sandbox-abc".to_string());
        task.fail("Something went wrong".to_string());
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.error_message.as_deref(), Some("Something went wrong"));
    }

    #[test]
    fn task_cancel_idempotent() {
        let mut task = new_task();

        let result = task.cancel();
        assert!(matches!(result, Idempotent::Executed(())));
        assert_eq!(task.status, TaskStatus::Cancelled);

        let result = task.cancel();
        assert!(matches!(result, Idempotent::AlreadyApplied));
    }
}
