mod config;

use std::sync::Arc;

use rmcp::model::{CallToolResult, Content, ErrorCode, ErrorData};

use concourse_client::ConcourseClient;

pub use config::ConcourseConfig;

/// Holds an initialized Concourse client for use by MCP tool handlers.
#[derive(Clone)]
pub struct ConcourseEndpoints {
    client: Arc<ConcourseClient>,
}

impl ConcourseEndpoints {
    /// Initialize from config. Returns `Ok(None)` if Concourse is disabled.
    pub fn try_new(config: &ConcourseConfig) -> Result<Option<Self>, anyhow::Error> {
        if !config.enabled {
            tracing::info!("Concourse integration disabled");
            return Ok(None);
        }

        if config.url.is_empty() {
            tracing::warn!("Concourse enabled but no URL configured — skipping");
            return Ok(None);
        }

        if config.username.is_empty() || config.password.is_empty() {
            tracing::warn!(
                "Concourse enabled but CONCOURSE_USERNAME/CONCOURSE_PASSWORD not set — skipping"
            );
            return Ok(None);
        }

        let client = ConcourseClient::new(
            &config.url,
            config.team.clone(),
            config.username.clone(),
            config.password.clone(),
        )?;

        tracing::info!(
            url = %config.url,
            team = %config.team,
            "Concourse integration initialized"
        );

        Ok(Some(Self {
            client: Arc::new(client),
        }))
    }

    /// List all pipelines across all accessible teams.
    pub async fn list_pipelines(&self) -> Result<CallToolResult, ErrorData> {
        let pipelines = self
            .client
            .list_all_pipelines()
            .await
            .map_err(concourse_err)?;

        let summary: Vec<serde_json::Value> = pipelines
            .iter()
            .map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "paused": p.paused,
                    "public": p.public,
                    "archived": p.archived,
                    "team": p.team_name,
                })
            })
            .collect();

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&summary).unwrap_or_default(),
        )]))
    }

    /// List jobs in a pipeline.
    pub async fn list_jobs(&self, pipeline: &str) -> Result<CallToolResult, ErrorData> {
        let jobs = self
            .client
            .list_jobs(pipeline)
            .await
            .map_err(concourse_err)?;

        let summary: Vec<serde_json::Value> = jobs
            .iter()
            .map(|j| {
                let mut obj = serde_json::json!({
                    "name": j.name,
                    "paused": j.paused,
                    "pipeline": j.pipeline_name,
                });
                if let Some(b) = &j.finished_build {
                    obj["last_status"] = serde_json::json!(b.status);
                }
                if let Some(b) = &j.next_build {
                    obj["current_build"] = serde_json::json!({
                        "id": b.id,
                        "status": b.status,
                    });
                }
                obj
            })
            .collect();

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&summary).unwrap_or_default(),
        )]))
    }

    /// Get the latest build status for a job.
    pub async fn get_build_status(
        &self,
        pipeline: &str,
        job: &str,
    ) -> Result<CallToolResult, ErrorData> {
        let builds = self
            .client
            .list_job_builds(pipeline, job, Some(1))
            .await
            .map_err(concourse_err)?;

        let Some(latest) = builds.first() else {
            return Ok(CallToolResult::success(vec![Content::text(
                "No builds found for this job.",
            )]));
        };

        let result = serde_json::json!({
            "build_id": latest.id,
            "name": latest.name,
            "status": latest.status,
            "pipeline": latest.pipeline_name,
            "job": latest.job_name,
            "start_time": latest.start_time,
            "end_time": latest.end_time,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Get build output/logs as plain text, returning the last `tail` lines.
    pub async fn get_build_logs(
        &self,
        build_id: i64,
        tail: usize,
    ) -> Result<CallToolResult, ErrorData> {
        let logs = self
            .client
            .get_build_logs(build_id)
            .await
            .map_err(concourse_err)?;

        if logs.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No log output available for this build.",
            )]));
        }

        let lines: Vec<&str> = logs.lines().collect();
        let output = if lines.len() > tail {
            let skipped = lines.len() - tail;
            format!(
                "... ({skipped} lines omitted, showing last {tail})\n{}",
                lines[lines.len() - tail..].join("\n")
            )
        } else {
            logs
        };

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Trigger a new build for a job.
    pub async fn trigger_build(
        &self,
        pipeline: &str,
        job: &str,
    ) -> Result<CallToolResult, ErrorData> {
        let build = self
            .client
            .trigger_build(pipeline, job)
            .await
            .map_err(concourse_err)?;

        let result = serde_json::json!({
            "build_id": build.id,
            "name": build.name,
            "status": build.status,
            "pipeline": build.pipeline_name,
            "job": build.job_name,
            "api_url": build.api_url,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Get pipeline config as a job dependency graph.
    pub async fn get_pipeline_config(&self, pipeline: &str) -> Result<CallToolResult, ErrorData> {
        let config = self
            .client
            .get_pipeline_config(pipeline)
            .await
            .map_err(concourse_err)?;

        // Build a job graph from the config
        let jobs: Vec<serde_json::Value> = config
            .config
            .jobs
            .iter()
            .map(|j| {
                // Extract get steps with passed/trigger from the plan
                let mut inputs = Vec::new();
                extract_get_steps(&j.plan, &mut inputs);
                serde_json::json!({
                    "name": j.name,
                    "inputs": inputs,
                })
            })
            .collect();

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&jobs).unwrap_or_default(),
        )]))
    }

    /// List recent builds for a job.
    pub async fn list_builds_for_job(
        &self,
        pipeline: &str,
        job: &str,
        limit: usize,
    ) -> Result<CallToolResult, ErrorData> {
        let builds = self
            .client
            .list_job_builds(pipeline, job, Some(limit))
            .await
            .map_err(concourse_err)?;

        let summary: Vec<serde_json::Value> = builds
            .iter()
            .map(|b| {
                serde_json::json!({
                    "build_id": b.id,
                    "name": b.name,
                    "status": b.status,
                    "start_time": b.start_time,
                    "end_time": b.end_time,
                })
            })
            .collect();

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&summary).unwrap_or_default(),
        )]))
    }

    /// Get resource versions that were inputs to a build.
    pub async fn get_build_resources(&self, build_id: i64) -> Result<CallToolResult, ErrorData> {
        let resources = self
            .client
            .get_build_resources(build_id)
            .await
            .map_err(concourse_err)?;

        let inputs: Vec<serde_json::Value> = resources
            .inputs
            .iter()
            .map(|i| {
                serde_json::json!({
                    "resource": i.name,
                    "version": i.version,
                    "first_occurrence": i.first_occurrence,
                })
            })
            .collect();

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&inputs).unwrap_or_default(),
        )]))
    }
}

/// Extract get steps (resource inputs) from a Concourse job plan.
fn extract_get_steps(plan: &[serde_json::Value], out: &mut Vec<serde_json::Value>) {
    for step in plan {
        if let Some(get) = step.get("get") {
            let mut input = serde_json::json!({
                "resource": step.get("resource").or(Some(get)).unwrap(),
            });
            if let Some(trigger) = step.get("trigger") {
                input["trigger"] = trigger.clone();
            }
            if let Some(passed) = step.get("passed") {
                input["passed"] = passed.clone();
            }
            out.push(input);
        }
        // Recurse into aggregate/in_parallel/do steps
        for key in &["aggregate", "in_parallel", "do"] {
            if let Some(nested) = step.get(key).and_then(|v| v.get("steps").or(Some(v))) {
                if let Some(arr) = nested.as_array() {
                    extract_get_steps(arr, out);
                }
            }
        }
    }
}

fn concourse_err(e: concourse_client::ConcourseError) -> ErrorData {
    ErrorData::new(
        ErrorCode::INTERNAL_ERROR,
        format!("Concourse API error: {e}"),
        None::<serde_json::Value>,
    )
}
