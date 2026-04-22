use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::auth::config::{AuthConfig, LoginMethod};
use drua_core::agent::{AgentsConfig, ModelDefaults};
use drua_core::prompt_executor::{
    ModelConfig, OpenAiResponsesAuth, PromptExecutorConfig, Provider,
};
use drua_core::sandbox::SandboxConfig;
use drua_core::toolset::ToolSetsConfig;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub oauth: OAuthConfig,
    #[serde(default)]
    pub db: DbConfig,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub agents: AgentsConfig,
    #[serde(default)]
    pub toolsets: ToolSetsConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub github_app: Option<GitHubAppCliConfig>,
    #[serde(skip)]
    pub anthropic_api_key: String,
    #[serde(skip)]
    pub openai_api_key: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    #[serde(default)]
    pub models: Vec<ProviderModelConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderModelConfig {
    pub name: String,
    pub max_tokens_per_response: u32,
    #[serde(default = "default_context_window")]
    pub context_window_tokens: u64,
    /// Provider prompt-cache TTL in seconds. Defaults to 300 (Anthropic).
    /// Set to 0 for providers without prompt caching.
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl_seconds: u64,
}

fn default_context_window() -> u64 {
    200_000
}

fn default_cache_ttl() -> u64 {
    300
}

impl Config {
    /// Build a `PromptExecutorConfig` from the top-level `providers`
    /// section, binding each model to its provider's API key.
    pub fn prompt_executor_config(&self) -> PromptExecutorConfig {
        let mut models = Vec::new();

        for provider_cfg in &self.providers {
            let provider = match provider_cfg.name.as_str() {
                "openai" => Provider::OpenAi {
                    api_key: self.openai_api_key.clone(),
                },
                "openai-responses" => Provider::OpenAiResponses {
                    auth: OpenAiResponsesAuth::ApiKey {
                        api_key: self.openai_api_key.clone(),
                    },
                },
                "openai-codex" => Provider::OpenAiResponses {
                    auth: OpenAiResponsesAuth::Subscription,
                },
                _ => Provider::Anthropic {
                    api_key: self.anthropic_api_key.clone(),
                },
            };

            for model in &provider_cfg.models {
                models.push(ModelConfig {
                    name: model.name.clone(),
                    provider: provider.clone(),
                    default_max_tokens: None,
                });
            }
        }

        PromptExecutorConfig { models }
    }
}

/// GitHub App config from the YAML config file.
/// The `private_key_path` field is `#[serde(skip)]` because it's a secret
/// loaded from an env var / K8s secret mount — never baked into the config file.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubAppCliConfig {
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub installation_id: String,
    /// Filesystem path to the PEM private key (loaded from env: GITHUB_APP_PRIVATE_KEY_PATH).
    #[serde(skip)]
    pub private_key_path: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_true")]
    pub secure_cookies: bool,
    #[serde(default = "default_mcp_endpoint")]
    pub mcp_endpoint: String,
}

fn default_port() -> u16 {
    4200
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}

fn default_true() -> bool {
    true
}

fn default_mcp_endpoint() -> String {
    "http://localhost:4200/mcp".to_string()
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: default_port(),
            host: default_host(),
            secure_cookies: true,
            mcp_endpoint: default_mcp_endpoint(),
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthConfig {
    #[serde(default)]
    pub login: LoginMethod,
    #[serde(default)]
    pub github_redirect_uri: String,
    #[serde(default)]
    pub github_client_id: String,
    #[serde(skip)]
    pub github_client_secret: String,
    #[serde(default)]
    pub github_allowed_teams: Vec<String>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbConfig {
    #[serde(skip)]
    pub pg_con: String,
}

pub struct EnvSecrets {
    pub pg_con: String,
    pub github_client_secret: String,
    pub github_allowed_teams: Vec<String>,
    pub anthropic_api_key: String,
    pub openai_api_key: String,
}

impl Config {
    pub fn try_new(
        path: impl AsRef<Path>,
        secrets: EnvSecrets,
        config_overrides: &[String],
    ) -> anyhow::Result<Self> {
        let config_file = std::fs::read_to_string(&path).map_err(|e| {
            anyhow::anyhow!(
                "Couldn't read config file {:?}: {}",
                path.as_ref().display(),
                e
            )
        })?;

        let mut yaml_value: serde_yaml::Value = serde_yaml::from_str(&config_file)
            .map_err(|e| anyhow::anyhow!("Invalid config: {e}"))?;

        for override_str in config_overrides {
            let (key, value) = override_str.split_once('=').context(format!(
                "Invalid override format '{override_str}', expected KEY=VALUE"
            ))?;
            apply_yaml_override(&mut yaml_value, key, value)?;
        }

        let mut config: Config = serde_yaml::from_value(yaml_value)
            .map_err(|e| anyhow::anyhow!("Invalid config: {e}"))?;

        // Populate agents.models from the top-level providers section.
        for provider_cfg in &config.providers {
            for model in &provider_cfg.models {
                config.agents.models.insert(
                    model.name.clone(),
                    ModelDefaults {
                        model: model.name.clone(),
                        max_tokens_per_response: model.max_tokens_per_response,
                        context_window_tokens: model.context_window_tokens,
                        cache_ttl_seconds: model.cache_ttl_seconds,
                    },
                );
            }
        }

        config.db.pg_con = secrets.pg_con;
        config.oauth.github_client_secret = secrets.github_client_secret;
        // Trim to catch stray whitespace / trailing newline that often
        // sneaks in when the key was piped from `echo` or a broken
        // `.env`; both render the key invalid upstream for opaque reasons.
        config.anthropic_api_key = secrets.anthropic_api_key.trim().to_string();
        config.openai_api_key = secrets.openai_api_key.trim().to_string();
        if !secrets.github_allowed_teams.is_empty() {
            config.oauth.github_allowed_teams = secrets.github_allowed_teams;
        }

        // Concourse toolset credentials from env
        if let Ok(val) = std::env::var("CONCOURSE_USERNAME") {
            config.toolsets.concourse.username = val;
        }
        if let Ok(val) = std::env::var("CONCOURSE_PASSWORD") {
            config.toolsets.concourse.password = val;
        }

        // Upstream MCP auth headers from env: {NAME}_AUTH_HEADER
        for upstream in &mut config.toolsets.mcp_upstreams {
            let env_key = format!("{}_AUTH_HEADER", upstream.name.to_uppercase());
            if let Ok(val) = std::env::var(&env_key) {
                upstream.auth_header = val;
            }
        }

        // GitHub App private key path from env (K8s secret mount)
        if let Some(ref mut gh) = config.github_app {
            if let Ok(val) = std::env::var("GITHUB_APP_PRIVATE_KEY_PATH") {
                gh.private_key_path = val;
            }
        }

        Ok(config)
    }

    pub fn auth_config(&self) -> AuthConfig {
        AuthConfig {
            login: self.oauth.login.clone(),
            github_client_id: self.oauth.github_client_id.clone(),
            github_client_secret: self.oauth.github_client_secret.clone(),
            github_redirect_uri: self.oauth.github_redirect_uri.clone(),
            github_allowed_teams: self.oauth.github_allowed_teams.clone(),
        }
    }
}

fn apply_yaml_override(root: &mut serde_yaml::Value, key: &str, value: &str) -> anyhow::Result<()> {
    let parts: Vec<&str> = key.split('.').collect();
    let mut current = root;

    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            let parsed_value = serde_yaml::from_str(value)
                .unwrap_or_else(|_| serde_yaml::Value::String(value.to_string()));
            current[*part] = parsed_value;
        } else {
            if !current.get(*part).is_some_and(|v| v.is_mapping()) {
                current[*part] = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
            }
            current = current
                .get_mut(*part)
                .context(format!("Failed to traverse config key '{key}'"))?;
        }
    }

    Ok(())
}
