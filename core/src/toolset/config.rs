use serde::{Deserialize, Serialize};

use crate::primitives::AuthScope;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ToolSetsConfig {
    #[serde(default)]
    pub mcp_upstreams: Vec<McpUpstreamConfig>,
    /// Log-shaped tool names per tunnel-registered toolset, keyed by the
    /// name the connector registers (e.g. `kubernetes`). A tunnel's
    /// catalog arrives at runtime from a remote connector that can't say
    /// what its tools' output looks like, so this stands in for the
    /// `log_tools` an [`McpUpstreamConfig`] would carry. Keyed by
    /// toolset rather than deployment, so a newly connected deployment
    /// inherits it instead of silently losing the tuning.
    #[serde(default)]
    pub tunnel_log_tools: std::collections::HashMap<String, Vec<String>>,
    #[serde(default)]
    pub concourse: ConcourseToolSetConfig,
    #[serde(default)]
    pub zenduty: ZendutyToolSetConfig,
    #[serde(default)]
    pub code_assistant: CodeAssistantToolSetConfig,
    #[serde(default)]
    pub compose: ComposeConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComposeConfig {
    #[serde(default)]
    pub script_step: ScriptStepLimits,
    #[serde(default = "default_max_script_file_bytes")]
    pub max_script_file_bytes: usize,
    #[serde(default = "default_max_script_total_bytes")]
    pub max_script_total_bytes: usize,
    #[serde(default = "default_max_script_loads")]
    pub max_script_loads: usize,
    #[serde(default = "default_max_script_dependency_depth")]
    pub max_script_dependency_depth: usize,

    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: usize,
    /// Cap on a single inner-tool result, in bytes.
    #[serde(default = "default_max_tool_result_bytes")]
    pub max_tool_result_bytes: usize,
    /// Cap on the script's final return value, in bytes.
    #[serde(default = "default_max_return_bytes")]
    pub max_return_bytes: usize,
    /// Cap on the console buffer in bytes. Tail-truncated when exceeded
    /// (oldest lines dropped first) so a runaway log loop never floods
    /// the agent context. ~1% of context at 8 KB.
    #[serde(default = "default_max_console_bytes")]
    pub max_console_bytes: usize,
    #[serde(default = "default_memory_limit_bytes")]
    pub memory_limit_bytes: usize,
    #[serde(default = "default_stack_limit_bytes")]
    pub stack_limit_bytes: usize,
    #[serde(
        default = "default_timeout_ms",
        alias = "default_timeout_ms",
        alias = "max_timeout_ms"
    )]
    pub timeout_ms: u64,
}

impl Default for ComposeConfig {
    fn default() -> Self {
        Self {
            script_step: ScriptStepLimits::default(),
            max_script_file_bytes: default_max_script_file_bytes(),
            max_script_total_bytes: default_max_script_total_bytes(),
            max_script_loads: default_max_script_loads(),
            max_script_dependency_depth: default_max_script_dependency_depth(),
            max_tool_calls: default_max_tool_calls(),
            max_tool_result_bytes: default_max_tool_result_bytes(),
            max_return_bytes: default_max_return_bytes(),
            max_console_bytes: default_max_console_bytes(),
            memory_limit_bytes: default_memory_limit_bytes(),
            stack_limit_bytes: default_stack_limit_bytes(),
            timeout_ms: default_timeout_ms(),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ScriptStepLimits {
    pub max_tool_calls: usize,
    pub timeout_ms: u64,
}

impl Default for ScriptStepLimits {
    fn default() -> Self {
        Self {
            max_tool_calls: 2000,
            timeout_ms: 1_800_000,
        }
    }
}

fn default_max_tool_calls() -> usize {
    50
}

fn default_max_tool_result_bytes() -> usize {
    // 32 MiB. Sized to comfortably hold realistic Concourse build logs
    // and similar large payloads. Compose's value proposition is reaching
    // *uncapped* data so scripts can filter / cross-reference in JS — the
    // cap exists for safety against pathological cases, not as a normal
    // throttle.
    32 * 1024 * 1024
}

fn default_max_return_bytes() -> usize {
    16 * 1024 * 1024
}

fn default_max_console_bytes() -> usize {
    8 * 1024
}

fn default_memory_limit_bytes() -> usize {
    // 64 MiB. Must remain a comfortable multiple of
    // `max_tool_result_bytes` (≥ 2×) so scripts have working room for
    // `.split()` / `.filter()` / sorting on top of the parsed payload.
    64 * 1024 * 1024
}

fn default_stack_limit_bytes() -> usize {
    512 * 1024
}

fn default_timeout_ms() -> u64 {
    420_000
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CodeAssistantToolSetConfig {
    #[serde(default)]
    pub db_path: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpAuthMode {
    #[default]
    Static,
    GithubApp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpUpstreamConfig {
    pub name: String,
    pub url: String,
    #[serde(skip)]
    pub auth_header: String,
    #[serde(default = "default_auth_header_name")]
    pub auth_header_name: String,
    /// When true (default), init fails if the auth header env var is missing.
    #[serde(default = "default_auth_required")]
    pub auth_required: bool,
    #[serde(default)]
    pub auth_mode: McpAuthMode,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub category_description: Option<String>,
    /// Defaults to `name` if unset.
    #[serde(default)]
    pub tool_prefix: Option<String>,
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// Unprefixed names of tools on this upstream that return a raw log
    /// or dump (`pods_log`, `nodes_log`, …). A remote MCP server can't
    /// tell drua what its output looks like, so this is where that gets
    /// declared: listed tools are summarised at any size instead of
    /// competing with tool-caching's min-hidden-bytes floor.
    #[serde(default)]
    pub log_tools: Vec<String>,
    /// Unprefixed names of tools that return a document the caller
    /// usually wants whole (`get_file_contents`, `pull_request_read`).
    /// Listed tools get tool-caching's document floor instead of the
    /// generic one, so they stay inline unless genuinely large.
    #[serde(default)]
    pub document_tools: Vec<String>,
    /// Empty means unrestricted.
    #[serde(default)]
    pub required_scopes: Option<Vec<AuthScope>>,
    /// When true, the toolset is hidden from non-agent subjects (Users,
    /// ExportedAgents, Anonymous). Use for upstreams whose tools should
    /// only be reachable from agents drua spawns itself (project_lead,
    /// agent, workflow_step_agent) — i.e. write-side surfaces like the
    /// GitHub PR mutation endpoint.
    #[serde(default)]
    pub internal_only: bool,
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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ZendutyToolSetConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Optional override for the API base URL. Defaults to
    /// `https://www.zenduty.com/` when empty.
    #[serde(default)]
    pub url: String,
    /// Default team unique_id used by schedule endpoints when none is
    /// supplied at call time.
    #[serde(default)]
    pub default_team: String,
    #[serde(skip)]
    pub api_token: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Helm chart renders these keys; a rename on either side would
    /// silently drop the declaration and put log tools back under the
    /// min-hidden-bytes floor.
    #[test]
    fn upstream_log_tools_parse_from_yaml() {
        let cfg: ToolSetsConfig = serde_yaml::from_str(
            r#"
mcp_upstreams:
  - name: kubernetes
    url: http://k8s-mcp:8080/mcp
    tool_prefix: k8s
    log_tools:
      - pods_log
      - nodes_log
"#,
        )
        .expect("upstream config with log_tools parses");
        assert_eq!(cfg.mcp_upstreams[0].log_tools, ["pods_log", "nodes_log"]);
        assert!(cfg.mcp_upstreams[0].document_tools.is_empty());
    }

    #[test]
    fn upstream_document_tools_parse_from_yaml() {
        let cfg: ToolSetsConfig = serde_yaml::from_str(
            r#"
mcp_upstreams:
  - name: github
    url: http://gh:8080/mcp
    tool_prefix: github
    document_tools:
      - get_file_contents
      - pull_request_read
"#,
        )
        .expect("upstream config with document_tools parses");
        assert_eq!(
            cfg.mcp_upstreams[0].document_tools,
            ["get_file_contents", "pull_request_read"]
        );
    }

    #[test]
    fn upstream_without_log_tools_defaults_to_empty() {
        let cfg: ToolSetsConfig = serde_yaml::from_str(
            r#"
mcp_upstreams:
  - name: github
    url: http://gh:8080/mcp
"#,
        )
        .expect("upstream config without log_tools parses");
        assert!(cfg.mcp_upstreams[0].log_tools.is_empty());
    }

    #[test]
    fn tunnel_log_tools_parse_from_yaml() {
        let cfg: ToolSetsConfig = serde_yaml::from_str(
            r#"
tunnel_log_tools:
  kubernetes:
    - pods_log
    - nodes_log
"#,
        )
        .expect("tunnel_log_tools parses");
        assert_eq!(
            cfg.tunnel_log_tools.get("kubernetes").unwrap(),
            &["pods_log".to_string(), "nodes_log".to_string()]
        );
    }
}

fn default_max_script_file_bytes() -> usize {
    262144
}
fn default_max_script_total_bytes() -> usize {
    1048576
}
fn default_max_script_loads() -> usize {
    64
}
fn default_max_script_dependency_depth() -> usize {
    16
}

#[cfg(test)]
mod script_limit_tests {
    use super::ComposeConfig;
    #[test]
    fn loader_defaults_preserve_existing_configs_and_allow_overrides() {
        let config: ComposeConfig = serde_json::from_str(r#"{"timeout_ms":123}"#).unwrap();
        assert_eq!(
            (
                config.max_script_file_bytes,
                config.max_script_total_bytes,
                config.max_script_loads,
                config.max_script_dependency_depth
            ),
            (262144, 1048576, 64, 16)
        );
        let config: ComposeConfig = serde_json::from_str(r#"{"max_script_loads":2}"#).unwrap();
        assert_eq!(config.max_script_loads, 2);
    }
}

#[cfg(test)]
mod script_step_tests {
    use super::*;

    #[test]
    fn workflow_limits_default_independently_from_agent_compose() {
        let config: ComposeConfig = serde_yaml::from_str("{}").unwrap();
        assert_eq!(config.max_tool_calls, 50);
        assert_eq!(config.timeout_ms, 420_000);
        assert_eq!(config.script_step.max_tool_calls, 2000);
        assert_eq!(config.script_step.timeout_ms, 1_800_000);
        let config: ComposeConfig =
            serde_yaml::from_str("script_step:\n  max_tool_calls: 80\n").unwrap();
        assert_eq!(config.script_step.max_tool_calls, 80);
        assert_eq!(config.script_step.timeout_ms, 1_800_000);
        assert_eq!(config.max_tool_calls, 50);
    }
}
