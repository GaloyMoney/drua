use std::sync::Arc;

use concourse_client::ConcourseClient;
use rmcp::model::{CallToolResult, Content, JsonObject, Tool};

use super::{ToolSet, ToolSetEntry, ToolSetsError};

pub struct ConcourseToolSet {
    client: Arc<ConcourseClient>,
    tools: Vec<ToolSetEntry>,
}

impl ConcourseToolSet {
    pub fn new(client: ConcourseClient) -> Self {
        let tools = vec![
            tool_entry(
                "list_pipelines",
                "List all accessible pipelines across all teams. Returns pipeline names, team, paused/archived status.",
                serde_json::json!({"type": "object", "properties": {}}),
            ),
            tool_entry(
                "list_jobs",
                "List jobs in a Concourse pipeline. Returns job names, paused state, and last build status.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pipeline": {"type": "string", "description": "The pipeline name to list jobs for"}
                    },
                    "required": ["pipeline"]
                }),
            ),
            tool_entry(
                "get_build_status",
                "Get the latest build status for a specific job in a Concourse pipeline. Returns build ID, status, and timestamps.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pipeline": {"type": "string", "description": "The pipeline name"},
                        "job": {"type": "string", "description": "The job name"}
                    },
                    "required": ["pipeline", "job"]
                }),
            ),
            tool_entry(
                "get_build_logs",
                "Get build output/logs for a Concourse build by its numeric build ID. Returns log output as plain text. For in-flight builds, returns partial output — use get_build_status first to check if the build has finished.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "build_id": {"type": "integer", "description": "The numeric build ID"},
                        "tail": {"type": "integer", "description": "Number of lines to return from the end of the log (default: 150)"}
                    },
                    "required": ["build_id"]
                }),
            ),
            tool_entry(
                "trigger_build",
                "Trigger a new build for a job in a Concourse pipeline. Returns the new build ID and status.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pipeline": {"type": "string", "description": "The pipeline name"},
                        "job": {"type": "string", "description": "The job name to trigger"}
                    },
                    "required": ["pipeline", "job"]
                }),
            ),
            tool_entry(
                "get_pipeline_config",
                "Get the job dependency graph for a Concourse pipeline. Returns each job's resource inputs with trigger and passed constraints, enabling critical path analysis from source to production.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pipeline": {"type": "string", "description": "The pipeline name"}
                    },
                    "required": ["pipeline"]
                }),
            ),
            tool_entry(
                "list_builds_for_job",
                "List recent builds for a job in a Concourse pipeline. Returns an array of builds (build_id, status, timestamps) ordered most recent first.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pipeline": {"type": "string", "description": "The pipeline name"},
                        "job": {"type": "string", "description": "The job name"},
                        "limit": {"type": "integer", "description": "Max number of recent builds to return (default: 10)"}
                    },
                    "required": ["pipeline", "job"]
                }),
            ),
            tool_entry(
                "get_build_resources",
                "Get the resource versions (e.g. git commit SHA) that were inputs to a Concourse build. Use this to correlate a commit to the builds it triggered.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "build_id": {"type": "integer", "description": "The numeric build ID"}
                    },
                    "required": ["build_id"]
                }),
            ),
        ];

        Self {
            client: Arc::new(client),
            tools,
        }
    }
}

#[async_trait::async_trait]
impl ToolSet for ConcourseToolSet {
    fn name(&self) -> &str {
        "concourse"
    }

    fn category(&self) -> &str {
        "ci"
    }

    fn category_description(&self) -> &str {
        "CI/CD pipelines, builds, and jobs"
    }

    fn tools(&self) -> &[ToolSetEntry] {
        &self.tools
    }

    async fn call(
        &self,
        tool_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let args = arguments.unwrap_or_default();
        match tool_name {
            "list_pipelines" => {
                let pipelines = self.client.list_all_pipelines().await?;
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
                Ok(text_result(&summary))
            }
            "list_jobs" => {
                let pipeline = str_arg(&args, "pipeline")?;
                let jobs = self.client.list_jobs(pipeline).await?;
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
                            obj["current_build"] =
                                serde_json::json!({"id": b.id, "status": b.status});
                        }
                        obj
                    })
                    .collect();
                Ok(text_result(&summary))
            }
            "get_build_status" => {
                let pipeline = str_arg(&args, "pipeline")?;
                let job = str_arg(&args, "job")?;
                let builds = self.client.list_job_builds(pipeline, job, Some(1)).await?;
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
            "get_build_logs" => {
                let build_id = int_arg(&args, "build_id")?;
                let tail = args.get("tail").and_then(|v| v.as_i64()).unwrap_or(150) as usize;
                let logs = self.client.get_build_logs(build_id).await?;
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
            "trigger_build" => {
                let pipeline = str_arg(&args, "pipeline")?;
                let job = str_arg(&args, "job")?;
                let build = self.client.trigger_build(pipeline, job).await?;
                let result = serde_json::json!({
                    "build_id": build.id,
                    "name": build.name,
                    "status": build.status,
                    "pipeline": build.pipeline_name,
                    "job": build.job_name,
                });
                Ok(CallToolResult::success(vec![Content::text(
                    serde_json::to_string_pretty(&result).unwrap_or_default(),
                )]))
            }
            "get_pipeline_config" => {
                let pipeline = str_arg(&args, "pipeline")?;
                let config = self.client.get_pipeline_config(pipeline).await?;
                let jobs: Vec<serde_json::Value> = config
                    .config
                    .jobs
                    .iter()
                    .map(|j| {
                        let mut inputs = Vec::new();
                        extract_get_steps(&j.plan, &mut inputs);
                        serde_json::json!({"name": j.name, "inputs": inputs})
                    })
                    .collect();
                Ok(text_result(&jobs))
            }
            "list_builds_for_job" => {
                let pipeline = str_arg(&args, "pipeline")?;
                let job = str_arg(&args, "job")?;
                let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(10) as usize;
                let builds = self
                    .client
                    .list_job_builds(pipeline, job, Some(limit))
                    .await?;
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
                Ok(text_result(&summary))
            }
            "get_build_resources" => {
                let build_id = int_arg(&args, "build_id")?;
                let resources = self.client.get_build_resources(build_id).await?;
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
                Ok(text_result(&inputs))
            }
            _ => Err(ToolSetsError::ToolNotFound(tool_name.to_string())),
        }
    }
}

fn tool_entry(name: &str, description: &str, schema: serde_json::Value) -> ToolSetEntry {
    let input_schema: JsonObject = match schema {
        serde_json::Value::Object(m) => m,
        _ => Default::default(),
    };
    let mut tool = Tool::default();
    tool.name = name.to_string().into();
    tool.description = Some(description.to_string().into());
    tool.input_schema = Arc::new(input_schema);
    ToolSetEntry {
        name: name.to_string(),
        description: tool,
    }
}

fn text_result(value: &[serde_json::Value]) -> CallToolResult {
    CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(value).unwrap_or_default(),
    )])
}

fn str_arg<'a>(args: &'a JsonObject, key: &str) -> Result<&'a str, ToolSetsError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolSetsError::MissingArgument(key.to_string()))
}

fn int_arg(args: &JsonObject, key: &str) -> Result<i64, ToolSetsError> {
    args.get(key)
        .and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .ok_or_else(|| ToolSetsError::MissingArgument(key.to_string()))
}

fn extract_get_steps(plan: &[serde_json::Value], out: &mut Vec<serde_json::Value>) {
    for step in plan {
        if let Some(get) = step.get("get") {
            let mut input = serde_json::json!({
                "resource": step.get("resource").unwrap_or(get),
            });
            if let Some(trigger) = step.get("trigger") {
                input["trigger"] = trigger.clone();
            }
            if let Some(passed) = step.get("passed") {
                input["passed"] = passed.clone();
            }
            out.push(input);
        }
        for key in &["aggregate", "in_parallel", "do"] {
            if let Some(nested) = step.get(key).and_then(|v| v.get("steps").or(Some(v))) {
                if let Some(arr) = nested.as_array() {
                    extract_get_steps(arr, out);
                }
            }
        }
    }
}
