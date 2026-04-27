use serde::{Deserialize, Serialize};

use crate::primitives::AuthScope;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ToolSetsConfig {
    #[serde(default)]
    pub mcp_upstreams: Vec<McpUpstreamConfig>,
    #[serde(default)]
    pub concourse: ConcourseToolSetConfig,
    #[serde(default)]
    pub code_assistant: CodeAssistantToolSetConfig,
    #[serde(default)]
    pub compose: ComposeConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComposeConfig {
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: usize,
    #[serde(default = "default_max_result_bytes")]
    pub max_result_bytes: usize,
    #[serde(default = "default_memory_limit_bytes")]
    pub memory_limit_bytes: usize,
    #[serde(default = "default_stack_limit_bytes")]
    pub stack_limit_bytes: usize,
    #[serde(default = "default_default_timeout_ms")]
    pub default_timeout_ms: u64,
    #[serde(default = "default_max_timeout_ms")]
    pub max_timeout_ms: u64,
}

impl Default for ComposeConfig {
    fn default() -> Self {
        Self {
            max_tool_calls: default_max_tool_calls(),
            max_result_bytes: default_max_result_bytes(),
            memory_limit_bytes: default_memory_limit_bytes(),
            stack_limit_bytes: default_stack_limit_bytes(),
            default_timeout_ms: default_default_timeout_ms(),
            max_timeout_ms: default_max_timeout_ms(),
        }
    }
}

fn default_max_tool_calls() -> usize {
    50
}

fn default_max_result_bytes() -> usize {
    100 * 1024
}

fn default_memory_limit_bytes() -> usize {
    8 * 1024 * 1024
}

fn default_stack_limit_bytes() -> usize {
    512 * 1024
}

fn default_default_timeout_ms() -> u64 {
    120_000
}

fn default_max_timeout_ms() -> u64 {
    300_000
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CodeAssistantToolSetConfig {
    #[serde(default)]
    pub db_path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpUpstreamConfig {
    pub name: String,
    pub url: String,
    #[serde(skip)]
    pub auth_header: String,
    #[serde(default = "default_auth_header_name")]
    pub auth_header_name: String,
    /// When true (default), the upstream requires authentication and init
    /// will fail early if the auth header env var is missing.
    #[serde(default = "default_auth_required")]
    pub auth_required: bool,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub category_description: Option<String>,
    /// Optional prefix for tool names (defaults to `name` if unset).
    #[serde(default)]
    pub tool_prefix: Option<String>,
    /// Optional whitelist of tool names to expose from this upstream.
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// Scopes required to access this upstream. Empty means unrestricted.
    #[serde(default)]
    pub required_scopes: Option<Vec<AuthScope>>,
}

fn default_auth_header_name() -> String {
    "authorization".to_string()
}

fn default_auth_required() -> bool {
    true
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConcourseToolSetConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub team: String,
    #[serde(skip)]
    pub username: String,
    #[serde(skip)]
    pub password: String,
}
