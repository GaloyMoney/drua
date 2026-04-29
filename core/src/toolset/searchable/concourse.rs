use std::sync::{Arc, LazyLock};

use concourse_client::ConcourseClient;
use rmcp::model::{CallToolResult, Content, JsonObject, Tool};
use serde::Deserialize;

use crate::auth::AuthSubject;

use super::super::filter::OutputFilter;
use super::super::{SearchableToolSet, ToolSetEntry, ToolSetsError};

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
        // `definitions` retained — compose TS generator resolves `$ref`s for recursive types.
        obj.insert(
            "additionalProperties".into(),
            serde_json::Value::Bool(false),
        );
    }
    value
}

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

#[derive(Deserialize, schemars::JsonSchema)]
struct PipelineParams {
    pipeline: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct PipelineJobParams {
    pipeline: String,
    job: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct BuildIdParams {
    #[serde(deserialize_with = "deserialize_liberal_i64")]
    build_id: i64,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ListBuildsParams {
    pipeline: String,
    job: String,
    /// Max recent builds to return (default 10).
    #[serde(default, deserialize_with = "deserialize_option_liberal_i64")]
    limit: Option<i64>,
}

static EMPTY_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false,
    })
});

static PIPELINE_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<PipelineParams>);
static PIPELINE_JOB_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<PipelineJobParams>);
static BUILD_ID_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<BuildIdParams>);
static LIST_BUILDS_SCHEMA: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<ListBuildsParams>);

#[derive(serde::Serialize, schemars::JsonSchema)]
struct PipelineOutput {
    name: String,
    paused: bool,
    public: bool,
    archived: bool,
    team: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct JobOutput {
    name: String,
    paused: bool,
    pipeline: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::toolset::any_json_schema")]
    current_build: Option<serde_json::Value>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct BuildStatusOutput {
    build_id: u64,
    name: String,
    status: String,
    pipeline: Option<String>,
    job: Option<String>,
    /// Build start time as Unix epoch **seconds** (multiply by 1000 for
    /// `new Date(start_time * 1000)` in JavaScript).
    start_time: Option<i64>,
    /// Build end time as Unix epoch **seconds** (multiply by 1000 for
    /// `new Date(end_time * 1000)` in JavaScript).
    end_time: Option<i64>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct BuildLogsOutput {
    logs: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct TriggerBuildOutput {
    build_id: u64,
    name: String,
    status: String,
    pipeline: Option<String>,
    job: Option<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct PipelineConfigJobOutput {
    name: String,
    #[schemars(schema_with = "crate::toolset::array_of_any_schema")]
    inputs: Vec<serde_json::Value>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct BuildSummaryOutput {
    build_id: u64,
    name: String,
    status: String,
    /// Build start time as Unix epoch **seconds** (multiply by 1000 for
    /// `new Date(start_time * 1000)` in JavaScript).
    start_time: Option<i64>,
    /// Build end time as Unix epoch **seconds** (multiply by 1000 for
    /// `new Date(end_time * 1000)` in JavaScript).
    end_time: Option<i64>,
}

#[cfg(test)]
mod schema_tests {
    use super::*;

    /// Doc comments on schemars-derived structs land in the rendered
    /// JSON Schema's `description` field for that property. They flow
    /// through to `compose_types` (TS signature for compose scripts) and
    /// `describe_tool` (raw JSON Schema), so agents see the time-unit
    /// guidance before writing scripts that touch these fields.
    fn description_for(schema: &serde_json::Value, field: &str) -> String {
        schema
            .get("properties")
            .and_then(|p| p.get(field))
            .and_then(|f| f.get("description"))
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn build_summary_time_fields_carry_unit_descriptions() {
        let schema = schema_for::<BuildSummaryOutput>();
        assert!(
            description_for(&schema, "start_time").contains("seconds"),
            "start_time description missing 'seconds' guidance: {schema:#}"
        );
        assert!(
            description_for(&schema, "end_time").contains("seconds"),
            "end_time description missing 'seconds' guidance: {schema:#}"
        );
    }

    #[test]
    fn build_status_time_fields_carry_unit_descriptions() {
        let schema = schema_for::<BuildStatusOutput>();
        assert!(description_for(&schema, "start_time").contains("seconds"));
        assert!(description_for(&schema, "end_time").contains("seconds"));
    }

    /// MCP `structuredContent` must be a JSON object — list-style outputs
    /// have to be wrapped in a named-field record, not a top-level array.
    /// Each `(static, field)` is the advertised `outputSchema` and the
    /// expected wrapper field name.
    #[test]
    fn list_outputs_are_object_wrapped_records() {
        for (schema, field) in [
            (&*OUT_LIST_PIPELINES, "pipelines"),
            (&*OUT_LIST_JOBS, "jobs"),
            (&*OUT_PIPELINE_CONFIG, "jobs"),
            (&*OUT_LIST_BUILDS, "builds"),
            (&*OUT_BUILD_RESOURCES, "resources"),
        ] {
            assert_eq!(
                schema.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "list outputSchema must be type:object, got: {schema:#}"
            );
            let prop = schema
                .get("properties")
                .and_then(|p| p.get(field))
                .unwrap_or_else(|| panic!("missing wrapper field `{field}` in: {schema:#}"));
            assert_eq!(
                prop.get("type").and_then(|v| v.as_str()),
                Some("array"),
                "wrapper field `{field}` should be an array, got: {prop:#}"
            );
        }
    }

    /// Strict MCP clients reject boolean schemas inside `properties` /
    /// `items`. `serde_json::Value` fields must serialize as `{}` — and
    /// `Vec<serde_json::Value>` as `array of {}` — never `true`.
    #[test]
    fn no_boolean_schemas_in_concourse_outputs() {
        fn assert_no_bool_schemas(value: &serde_json::Value, path: &str) {
            match value {
                serde_json::Value::Object(map) => {
                    if let Some(props) = map.get("properties").and_then(|v| v.as_object()) {
                        for (k, v) in props {
                            assert!(
                                !matches!(v, serde_json::Value::Bool(_)),
                                "boolean schema at {path}.properties.{k}"
                            );
                            assert_no_bool_schemas(v, &format!("{path}.properties.{k}"));
                        }
                    }
                    if let Some(items) = map.get("items") {
                        assert!(
                            !matches!(items, serde_json::Value::Bool(_)),
                            "boolean schema at {path}.items"
                        );
                        assert_no_bool_schemas(items, &format!("{path}.items"));
                    }
                }
                _ => {}
            }
        }
        for (name, schema) in [
            ("OUT_LIST_PIPELINES", &*OUT_LIST_PIPELINES),
            ("OUT_LIST_JOBS", &*OUT_LIST_JOBS),
            ("OUT_PIPELINE_CONFIG", &*OUT_PIPELINE_CONFIG),
            ("OUT_LIST_BUILDS", &*OUT_LIST_BUILDS),
            ("OUT_BUILD_RESOURCES", &*OUT_BUILD_RESOURCES),
        ] {
            assert_no_bool_schemas(schema, name);
        }
    }
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct BuildResourceOutput {
    resource: String,
    #[schemars(schema_with = "crate::toolset::any_json_schema")]
    version: serde_json::Value,
    first_occurrence: bool,
}

// List-shaped outputs are wrapped in a named-field object so that
// `structuredContent` is a JSON object (per the MCP spec) rather than a
// top-level array.

#[derive(serde::Serialize, schemars::JsonSchema)]
struct PipelineListOutput {
    pipelines: Vec<PipelineOutput>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct JobListOutput {
    jobs: Vec<JobOutput>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct PipelineConfigOutput {
    jobs: Vec<PipelineConfigJobOutput>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct BuildListOutput {
    builds: Vec<BuildSummaryOutput>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct BuildResourceListOutput {
    resources: Vec<BuildResourceOutput>,
}

static OUT_LIST_PIPELINES: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<PipelineListOutput>);
static OUT_LIST_JOBS: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<JobListOutput>);
static OUT_BUILD_STATUS: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<BuildStatusOutput>);
static OUT_BUILD_LOGS: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<BuildLogsOutput>);
static OUT_TRIGGER_BUILD: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<TriggerBuildOutput>);
static OUT_PIPELINE_CONFIG: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<PipelineConfigOutput>);
static OUT_LIST_BUILDS: LazyLock<serde_json::Value> = LazyLock::new(schema_for::<BuildListOutput>);
static OUT_BUILD_RESOURCES: LazyLock<serde_json::Value> =
    LazyLock::new(schema_for::<BuildResourceListOutput>);

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
                (*OUT_LIST_PIPELINES).clone(),
            ),
            tool_entry(
                "list_jobs",
                "List jobs in a Concourse pipeline. Returns job names, paused state, and last build status.",
                (*PIPELINE_SCHEMA).clone(),
                (*OUT_LIST_JOBS).clone(),
            ),
            tool_entry(
                "get_build_status",
                "Get the latest build status for a specific job in a Concourse pipeline. Returns build ID, status, and timestamps.",
                (*PIPELINE_JOB_SCHEMA).clone(),
                (*OUT_BUILD_STATUS).clone(),
            ),
            tool_entry_with_filter(
                "get_build_logs",
                "Get build output/logs for a Concourse build by its numeric build ID. Returns log output as plain text. For in-flight builds, returns partial output — use get_build_status first to check if the build has finished. Output filtering (grep, tail, head) is handled by call_tool's output_filter parameter; default: tail 150 lines.",
                (*BUILD_ID_SCHEMA).clone(),
                (*OUT_BUILD_LOGS).clone(),
                Some(OutputFilter {
                    tail: Some(150),
                    ..Default::default()
                }),
            ),
            tool_entry(
                "trigger_build",
                "Trigger a new build for a job in a Concourse pipeline. Returns the new build ID and status.",
                (*PIPELINE_JOB_SCHEMA).clone(),
                (*OUT_TRIGGER_BUILD).clone(),
            ),
            tool_entry(
                "get_pipeline_config",
                "Get the job dependency graph for a Concourse pipeline. Returns each job's resource inputs with trigger and passed constraints, enabling critical path analysis from source to production.",
                (*PIPELINE_SCHEMA).clone(),
                (*OUT_PIPELINE_CONFIG).clone(),
            ),
            tool_entry(
                "list_builds_for_job",
                "List recent builds for a job in a Concourse pipeline. Returns an array of builds (build_id, status, timestamps) ordered most recent first.",
                (*LIST_BUILDS_SCHEMA).clone(),
                (*OUT_LIST_BUILDS).clone(),
            ),
            tool_entry(
                "get_build_resources",
                "Get the resource versions (e.g. git commit SHA) that were inputs to a Concourse build. Use this to correlate a commit to the builds it triggered.",
                (*BUILD_ID_SCHEMA).clone(),
                (*OUT_BUILD_RESOURCES).clone(),
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
                let out = PipelineListOutput {
                    pipelines: pipelines
                        .iter()
                        .map(|p| PipelineOutput {
                            name: p.name.clone(),
                            paused: p.paused,
                            public: p.public,
                            archived: p.archived,
                            team: p.team_name.clone(),
                        })
                        .collect(),
                };
                Ok(typed_result(&out))
            }
            "list_jobs" => {
                let params: PipelineParams = parse_params(arguments)?;
                let jobs = self.client.list_jobs(&params.pipeline).await?;
                let out = JobListOutput {
                    jobs: jobs
                        .iter()
                        .map(|j| JobOutput {
                            name: j.name.clone(),
                            paused: j.paused,
                            pipeline: j.pipeline_name.clone(),
                            last_status: j.finished_build.as_ref().map(|b| b.status.clone()),
                            current_build: j
                                .next_build
                                .as_ref()
                                .map(|b| serde_json::json!({"id": b.id, "status": b.status})),
                        })
                        .collect(),
                };
                Ok(typed_result(&out))
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
                let out = BuildStatusOutput {
                    build_id: latest.id,
                    name: latest.name.clone(),
                    status: latest.status.clone(),
                    pipeline: latest.pipeline_name.clone(),
                    job: latest.job_name.clone(),
                    start_time: latest.start_time,
                    end_time: latest.end_time,
                };

                Ok(typed_result(&out))
            }
            "get_build_logs" => {
                let params: BuildIdParams = parse_params(arguments)?;
                let logs = self.client.get_build_logs(params.build_id).await?;
                let text = if logs.is_empty() {
                    "No log output available for this build.".to_string()
                } else {
                    logs.clone()
                };
                let out = BuildLogsOutput { logs };
                let structured = serde_json::to_value(&out).expect("serialization");
                let mut result = CallToolResult::success(vec![Content::text(text)]);
                result.structured_content = Some(structured);
                Ok(result)
            }
            "trigger_build" => {
                let params: PipelineJobParams = parse_params(arguments)?;
                let build = self
                    .client
                    .trigger_build(&params.pipeline, &params.job)
                    .await?;
                let out = TriggerBuildOutput {
                    build_id: build.id,
                    name: build.name.clone(),
                    status: build.status.clone(),
                    pipeline: build.pipeline_name.clone(),
                    job: build.job_name.clone(),
                };

                Ok(typed_result(&out))
            }
            "get_pipeline_config" => {
                let params: PipelineParams = parse_params(arguments)?;
                let config = self.client.get_pipeline_config(&params.pipeline).await?;
                let out = PipelineConfigOutput {
                    jobs: config
                        .config
                        .jobs
                        .iter()
                        .map(|j| {
                            let mut inputs = Vec::new();
                            extract_get_steps(&j.plan, &mut inputs);
                            PipelineConfigJobOutput {
                                name: j.name.clone(),
                                inputs,
                            }
                        })
                        .collect(),
                };
                Ok(typed_result(&out))
            }
            "list_builds_for_job" => {
                let params: ListBuildsParams = parse_params(arguments)?;
                let limit = params.limit.unwrap_or(10) as usize;
                let builds = self
                    .client
                    .list_job_builds(&params.pipeline, &params.job, Some(limit))
                    .await?;
                let out = BuildListOutput {
                    builds: builds
                        .iter()
                        .map(|b| BuildSummaryOutput {
                            build_id: b.id,
                            name: b.name.clone(),
                            status: b.status.clone(),
                            start_time: b.start_time,
                            end_time: b.end_time,
                        })
                        .collect(),
                };
                Ok(typed_result(&out))
            }
            "get_build_resources" => {
                let params: BuildIdParams = parse_params(arguments)?;
                let resources = self.client.get_build_resources(params.build_id).await?;
                let out = BuildResourceListOutput {
                    resources: resources
                        .inputs
                        .iter()
                        .map(|i| BuildResourceOutput {
                            resource: i.name.clone(),
                            version: i.version.clone(),
                            first_occurrence: i.first_occurrence,
                        })
                        .collect(),
                };
                Ok(typed_result(&out))
            }
            _ => Err(ToolSetsError::ToolNotFound(tool_name.to_string())),
        }
    }
}

fn tool_entry(
    name: &str,
    description: &str,
    schema: serde_json::Value,
    output_schema: serde_json::Value,
) -> ToolSetEntry {
    tool_entry_with_filter(name, description, schema, output_schema, None)
}

fn tool_entry_with_filter(
    name: &str,
    description: &str,
    schema: serde_json::Value,
    output_schema: serde_json::Value,
    default_output_filter: Option<OutputFilter>,
) -> ToolSetEntry {
    let input_schema: JsonObject = match schema {
        serde_json::Value::Object(m) => m,
        _ => Default::default(),
    };
    let out_schema = match output_schema {
        serde_json::Value::Object(m) => Some(Arc::new(m)),
        _ => None,
    };
    let mut tool = Tool::default();
    tool.name = name.to_string().into();
    tool.description = Some(description.to_string().into());
    tool.input_schema = Arc::new(input_schema);
    tool.output_schema = out_schema;
    ToolSetEntry {
        name: name.to_string(),
        description: tool,
        default_output_filter,
    }
}

fn typed_result<T: serde::Serialize>(value: &T) -> CallToolResult {
    let structured = serde_json::to_value(value).expect("serialization");
    let text = serde_json::to_string_pretty(&structured).unwrap_or_default();
    let mut result = CallToolResult::success(vec![Content::text(text)]);
    result.structured_content = Some(structured);
    result
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
