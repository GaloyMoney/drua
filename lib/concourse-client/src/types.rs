use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub type InstanceVars = HashMap<String, serde_json::Value>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pipeline {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub instance_vars: Option<InstanceVars>,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub public: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub team_name: String,
    #[serde(default)]
    pub last_updated: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub team_name: String,
    #[serde(default)]
    pub pipeline_id: u64,
    #[serde(default)]
    pub pipeline_name: String,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub has_new_inputs: bool,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub disable_manual_trigger: bool,
    #[serde(default)]
    pub next_build: Option<Build>,
    #[serde(default)]
    pub finished_build: Option<Build>,
    #[serde(default)]
    pub transition_build: Option<Build>,
    #[serde(default)]
    pub inputs: Vec<JobInput>,
    #[serde(default)]
    pub outputs: Vec<JobOutput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobInput {
    pub name: String,
    #[serde(default)]
    pub resource: String,
    #[serde(default)]
    pub trigger: bool,
    #[serde(default)]
    pub passed: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobOutput {
    pub name: String,
    #[serde(default)]
    pub resource: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Build {
    pub id: u64,
    #[serde(default)]
    pub team_name: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub api_url: String,
    #[serde(default)]
    pub job_name: Option<String>,
    #[serde(default)]
    pub pipeline_id: Option<u64>,
    #[serde(default)]
    pub pipeline_name: Option<String>,
    #[serde(default)]
    pub pipeline_instance_vars: Option<InstanceVars>,
    #[serde(default)]
    pub start_time: Option<i64>,
    #[serde(default)]
    pub end_time: Option<i64>,
    #[serde(default)]
    pub created_by: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PipelineConfigResponse {
    pub config: PipelineConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineConfig {
    #[serde(default)]
    pub jobs: Vec<PipelineJobConfig>,
    #[serde(default)]
    pub resources: Vec<PipelineResourceConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineJobConfig {
    pub name: String,
    #[serde(default)]
    pub plan: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineResourceConfig {
    pub name: String,
    #[serde(rename = "type")]
    pub resource_type: String,
    #[serde(default)]
    pub source: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildResources {
    #[serde(default)]
    pub inputs: Vec<BuildResourceInput>,
    #[serde(default)]
    pub outputs: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildResourceInput {
    pub name: String,
    #[serde(default)]
    pub version: serde_json::Value,
    #[serde(default)]
    pub pipeline_id: u64,
    #[serde(default)]
    pub first_occurrence: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildEventEnvelope {
    pub data: serde_json::Value,
    pub event: String,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub token_type: String,
    #[serde(default)]
    pub expires_in: Option<u64>,
}
