use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tracing::instrument;

use domain::primitives::TaskId;
use domain::task::{TaskStatus, Tasks};
use galoy_agents_core as domain;
use sandbox_client::SandboxClient;

use crate::TaskGithubTokens;

/// Configuration for the task runner.
#[derive(Clone)]
pub struct TaskRunnerConfig {
    /// How often to poll for pending tasks.
    pub poll_interval: Duration,
    /// Timeout for sandbox to become ready.
    pub sandbox_timeout: Duration,
    /// Base URL for the callback endpoint (e.g. "http://localhost:4200").
    pub callback_base_url: String,
    /// ANTHROPIC_API_KEY to inject into sandbox pods.
    pub anthropic_api_key: Option<String>,
}

impl Default for TaskRunnerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(5),
            sandbox_timeout: Duration::from_secs(120),
            callback_base_url: "http://localhost:4200".to_string(),
            anthropic_api_key: None,
        }
    }
}

/// Background service that picks up pending tasks and runs them in sandboxes.
pub struct TaskRunner {
    tasks: Tasks,
    sandbox: Arc<SandboxClient>,
    github_tokens: TaskGithubTokens,
    config: TaskRunnerConfig,
}

impl TaskRunner {
    pub fn new(
        tasks: Tasks,
        sandbox: Arc<SandboxClient>,
        github_tokens: TaskGithubTokens,
        config: TaskRunnerConfig,
    ) -> Self {
        Self {
            tasks,
            sandbox,
            github_tokens,
            config,
        }
    }

    /// Start the runner loop. Runs until the token is cancelled.
    pub async fn run(self, cancel: tokio::sync::watch::Receiver<bool>) {
        tracing::info!("Task runner started");
        loop {
            if *cancel.borrow() {
                tracing::info!("Task runner shutting down");
                break;
            }

            if let Err(e) = self.poll_once().await {
                tracing::error!(error = %e, "Task runner poll error");
            }

            tokio::time::sleep(self.config.poll_interval).await;
        }
    }

    #[instrument(name = "task_runner.poll_once", skip_all)]
    async fn poll_once(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let pending = self.tasks.list_by_status(TaskStatus::Pending).await?;

        for task in pending {
            let runner = self.clone_for_task();
            let task_id = task.id;
            tokio::spawn(async move {
                if let Err(e) = runner.execute_task(task_id).await {
                    tracing::error!(
                        task_id = %task_id,
                        error = %e,
                        "Task execution failed"
                    );
                    // Try to mark the task as failed
                    let _ = runner.tasks.fail(task_id, e.to_string()).await;
                }
            });
        }

        Ok(())
    }

    fn clone_for_task(&self) -> Self {
        Self {
            tasks: self.tasks.clone(),
            sandbox: self.sandbox.clone(),
            github_tokens: self.github_tokens.clone(),
            config: self.config.clone(),
        }
    }

    #[instrument(name = "task_runner.execute_task", skip(self), fields(%task_id))]
    async fn execute_task(
        &self,
        task_id: TaskId,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let task = self.tasks.find_by_id(task_id).await?;

        // Double-check status (another runner may have picked it up)
        if task.status != TaskStatus::Pending {
            return Ok(());
        }

        let sandbox_name = format!("task-{}", task_id);

        // 1. Create sandbox
        tracing::info!(task_id = %task_id, sandbox = %sandbox_name, "Creating sandbox for task");
        self.sandbox.create_claim(&sandbox_name).await?;

        // Ensure cleanup on any exit path
        let cleanup_sandbox = self.sandbox.clone();
        let cleanup_name = sandbox_name.clone();
        let _cleanup_guard = scopeguard::guard((), |_| {
            let sandbox = cleanup_sandbox;
            let name = cleanup_name;
            tokio::spawn(async move {
                tracing::info!(sandbox = %name, "Cleaning up sandbox");
                if let Err(e) = sandbox.delete_claim(&name).await {
                    tracing::warn!(error = %e, sandbox = %name, "Failed to cleanup sandbox");
                }
            });
        });

        // 2. Wait for sandbox to be ready
        tracing::info!(task_id = %task_id, "Waiting for sandbox to be ready");
        self.sandbox
            .wait_until_ready(&sandbox_name, self.config.sandbox_timeout)
            .await?;

        // 3. Mark task as running
        self.tasks.start(task_id, sandbox_name.clone()).await?;

        // 4. Build the command with env vars
        let github_token = self.github_tokens.write().await.remove(&task_id);
        let description = task.description.clone();

        // Build env var setup and claude command
        let mut env_setup = String::new();
        if let Some(key) = &self.config.anthropic_api_key {
            env_setup.push_str(&format!("export ANTHROPIC_API_KEY='{}'; ", key));
        }
        if let Some(token) = &github_token {
            env_setup.push_str(&format!("export GITHUB_TOKEN='{}'; ", token));
        }

        // Escape single quotes in description for shell
        let escaped_desc = description.replace('\'', "'\\''");
        let command = format!(
            "{}claude -p '{}' --output-format stream-json 2>&1; echo \"EXIT_CODE=$?\"",
            env_setup, escaped_desc
        );

        // 5. Exec into sandbox
        tracing::info!(task_id = %task_id, "Executing claude CLI in sandbox");
        let mut process = self
            .sandbox
            .exec_in_sandbox(&sandbox_name, vec!["sh".into(), "-c".into(), command])
            .await?;

        // 6. Stream stdout output
        let mut stdout = process.stdout().ok_or("No stdout from sandbox exec")?;

        let mut buf = [0u8; 8192];
        let mut exit_code: Option<i32> = None;
        let mut line_buf = String::new();

        loop {
            match stdout.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]);
                    line_buf.push_str(&chunk);

                    // Process complete lines
                    while let Some(newline_pos) = line_buf.find('\n') {
                        let line = line_buf[..newline_pos].trim_end_matches('\r').to_string();
                        line_buf = line_buf[newline_pos + 1..].to_string();

                        // Check for exit code marker
                        if let Some(code_str) = line.strip_prefix("EXIT_CODE=") {
                            exit_code = code_str.trim().parse().ok();
                            continue;
                        }

                        // Store output line
                        if let Err(e) = self.tasks.receive_output(task_id, line).await {
                            tracing::warn!(error = %e, "Failed to store output line");
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Error reading sandbox stdout");
                    break;
                }
            }
        }

        // Process any remaining partial line
        if !line_buf.is_empty() {
            let line = line_buf.trim().to_string();
            if let Some(code_str) = line.strip_prefix("EXIT_CODE=") {
                exit_code = code_str.trim().parse().ok();
            } else if !line.is_empty() {
                let _ = self.tasks.receive_output(task_id, line).await;
            }
        }

        // 7. Complete or fail based on exit code
        match exit_code {
            Some(0) => {
                tracing::info!(task_id = %task_id, "Task completed successfully");
                // TODO: parse git_branch and pr_url from output
                self.tasks.complete(task_id, None, None).await?;
            }
            Some(code) => {
                tracing::warn!(task_id = %task_id, exit_code = code, "Task failed with non-zero exit");
                self.tasks
                    .fail(task_id, format!("Process exited with code {code}"))
                    .await?;
            }
            None => {
                tracing::warn!(task_id = %task_id, "Task exited without exit code");
                self.tasks
                    .fail(task_id, "Process exited without exit code".to_string())
                    .await?;
            }
        }

        Ok(())
    }
}
