use std::sync::{Arc, LazyLock};

use concourse_client::ConcourseClient;
use rmcp::model::{CallToolResult, Content, JsonObject, Tool};
use serde::Deserialize;

use crate::auth::AuthSubject;

use super::super::filter::OutputFilter;
use super::super::{SearchableToolSet, ToolSetEntry, ToolSetsError};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_params<T: serde::de::DeserializeOwned>(
    arguments: Option<JsonObject>,
) -> Result<T, ToolSetsError> {
    let value = serde_json::Value::Object(arguments.unwrap_or_default());
    serde_json::from_value(value).map_err(|e| ToolSetsError::InvalidArgument(e.to_string()))
}

fn schema_for<T: schemars::JsonSchema>() -> serde_json::Value {
    let settings = schemars::gen::SchemaSettings::draft07().with(|s| {
        s.inline_subschemas = true;
        s.meta_schema = None;
    });
    let generator = settings.into_generator();
    let schema = generator.into_root_schema_for::<T>();
    let mut value = serde_json::to_value(schema).expect("schema serialization");
    if let Some(obj) = value.as_object_mut() {
        obj.remove("title");
        obj.remove("definitions");
        obj.insert(
            "additionalProperties".into(),
            serde_json::Value::Bool(false),
        );
    }
    value
}

/// Deserialize an `i64` from either a JSON number or a string like `"20"`.
fn deserialize_liberal_i64<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<i64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrInt {
        Int(i64),
        Str(String),
    }
    match StringOrInt::deserialize(deserializer)? {
        StringOrInt::Int(v) => Ok(v),
        StringOrInt::Str(s) => s.parse().map_err(serde::de::Error::custom),
    }
}

/// Deserialize an `Option<i64>` from a JSON number, string, or null.
fn deserialize_option_liberal_i64<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<i64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrInt {
        Int(i64),
        Str(String),
    }
    let opt: Option<StringOrInt> = Option::deserialize(deserializer)?;
    match opt {
        None => Ok(None),
        Some(StringOrInt::Int(v)) => Ok(Some(v)),
        Some(StringOrInt::Str(s)) => s.parse().map(Some).map_err(serde::de::Error::custom),
    }
}

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
struct PipelineParams {
    /// The pipeline name.
    pipeline: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct PipelineJobParams {
    /// The pipeline name.
    pipeline: String,
    /// The job name.
    job: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct BuildIdParams {
    /// The numeric build ID.
    #[serde(deserialize_with = "deserialize_liberal_i64")]
    build_id: i64,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ListBuildsParams {
    /// The pipeline name.
    pipeline: String,
    /// The job name.
    job: String,
    /// Max number of recent builds to return (default: 10).
    #[serde(default, deserialize_with = "deserialize_option_liberal_i64")]
    limit: Option<i64>,
}

// ---------------------------------------------------------------------------
// Schemas
// ---------------------------------------------------------------------------

static EMPTY_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false,
    })
});

static PIPELINE_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<PipelineParams>);
static PIPELINE_JOB_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<PipelineJobParams>);
static BUILD_ID_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<BuildIdParams>);
static LIST_BUILDS_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<ListBuildsParams>);

// ---------------------------------------------------------------------------
// Toolset
// ---------------------------------------------------------------------------

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
                (*EMPTY_SCHEMA).clone(),
            ),
            tool_entry(
                "list_jobs",
                "List jobs in a Concourse pipeline. Returns job names, paused state, and last build status.",
                (*PIPELINE_SCHEMA).clone(),
            ),
            tool_entry(
                "get_build_status",
                "Get the latest build status for a specific job in a Concourse pipeline. Returns build ID, status, and timestamps.",
                (*PIPELINE_JOB_SCHEMA).clone(),
            ),
            tool_entry_with_filter(
                "get_build_logs",
                "Get build output/logs for a Concourse build by its numeric build ID. Returns log output as plain text. For in-flight builds, returns partial output — use get_build_status first to check if the build has finished. Output filtering (grep, tail, head) is handled by call_tool's output_filter parameter; default: tail 150 lines.",
                (*BUILD_ID_SCHEMA).clone(),
                Some(OutputFilter {
                    tail: Some(150),
                    ..Default::default()
                }),
            ),
            tool_entry(
                "trigger_build",
                "Trigger a new build for a job in a Concourse pipeline. Returns the new build ID and status.",
                (*PIPELINE_JOB_SCHEMA).clone(),
            ),
            tool_entry(
                "get_pipeline_config",
                "Get the job dependency graph for a Concourse pipeline. Returns each job's resource inputs with trigger and passed constraints, enabling critical path analysis from source to production.",
                (*PIPELINE_SCHEMA).clone(),
            ),
            tool_entry(
                "list_builds_for_job",
                "List recent builds for a job in a Concourse pipeline. Returns an array of builds (build_id, status, timestamps) ordered most recent first.",
                (*LIST_BUILDS_SCHEMA).clone(),
            ),
            tool_entry(
                "get_build_resources",
                "Get the resource versions (e.g. git commit SHA) that were inputs to a Concourse build. Use this to correlate a commit to the builds it triggered.",
                (*BUILD_ID_SCHEMA).clone(),
            ),
        ];

        Self {
            client: Arc::new(client),
            tools,
        }
    }
}

#[async_trait::async_trait]
impl SearchableToolSet for ConcourseToolSet {
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
        _subject: &AuthSubject,
        tool_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
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
                let params: PipelineParams = parse_params(arguments)?;
                let jobs = self.client.list_jobs(&params.pipeline).await?;
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
                let params: PipelineJobParams = parse_params(arguments)?;
                let builds = self
                    .client
                    .list_job_builds(&params.pipeline, &params.job, Some(1))
                    .await?;
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
                let params: BuildIdParams = parse_params(arguments)?;
                let logs = self.client.get_build_logs(params.build_id).await?;
                if logs.is_empty() {
                    return Ok(CallToolResult::success(vec![Content::text(
                        "No log output available for this build.",
                    )]));
                }
                Ok(CallToolResult::success(vec![Content::text(logs)]))
            }
            "trigger_build" => {
                let params: PipelineJobParams = parse_params(arguments)?;
                let build = self
                    .client
                    .trigger_build(&params.pipeline, &params.job)
                    .await?;
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
                let params: PipelineParams = parse_params(arguments)?;
                let config = self.client.get_pipeline_config(&params.pipeline).await?;
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
                let params: ListBuildsParams = parse_params(arguments)?;
                let limit = params.limit.unwrap_or(10) as usize;
                let builds = self
                    .client
                    .list_job_builds(&params.pipeline, &params.job, Some(limit))
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
                let params: BuildIdParams = parse_params(arguments)?;
                let resources = self.client.get_build_resources(params.build_id).await?;
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
    tool_entry_with_filter(name, description, schema, None)
}

fn tool_entry_with_filter(
    name: &str,
    description: &str,
    schema: serde_json::Value,
    default_output_filter: Option<OutputFilter>,
) -> ToolSetEntry {
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
        default_output_filter,
    }
}

fn text_result(value: &[serde_json::Value]) -> CallToolResult {
    CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(value).unwrap_or_default(),
    )])
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
