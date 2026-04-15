use std::path::Path;

use serde::Deserialize;

use galoy_agents_core::agent::AgentsConfig;
use galoy_agents_core::mcp_jwt::McpJwtConfig;
use galoy_agents_core::prompt_executor::{ModelConfig, PromptExecutorConfig, Provider};
use galoy_agents_core::sandbox::SandboxConfig;
use galoy_agents_core::toolset::ToolSetsConfig;
use galoy_agents_web::auth::config::AuthConfig;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub oauth: OAuthConfig,
    #[serde(default)]
    pub db: DbConfig,
    #[serde(default)]
    pub agents: AgentsConfig,
    #[serde(default)]
    pub toolsets: ToolSetsConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub github_app: Option<GitHubAppCliConfig>,
    #[serde(default)]
    pub mcp_jwt: Option<McpJwtCliConfig>,
    #[serde(skip)]
    pub anthropic_api_key: String,
}

impl Config {
    /// Build a `PromptExecutorConfig` registering every model that the
    /// configured `agents.builtin_roles` reference, all bound to Anthropic
    /// using the API key provided via env.
    pub fn prompt_executor_config(&self) -> PromptExecutorConfig {
        let mut models: Vec<String> = self
            .agents
            .builtin_roles
            .values()
            .map(|r| r.model.clone())
            .collect();
        models.sort();
        models.dedup();
        PromptExecutorConfig {
            models: models
                .into_iter()
                .map(|name| ModelConfig {
                    name,
                    provider: Provider::Anthropic {
                        api_key: self.anthropic_api_key.clone(),
                    },
                    default_max_tokens: None,
                })
                .collect(),
        }
    }
}

/// GitHub App config from the YAML config file.
/// The `private_key_path` field is `#[serde(skip)]` because it's a secret
/// loaded from an env var / K8s secret mount — never baked into the config file.
#[derive(Clone, Default, Deserialize)]
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

/// MCP JWT signer config from the YAML config file.
/// `private_key_path` is loaded from env (MCP_JWT_PRIVATE_KEY_PATH) to
/// point at the K8s secret mount.
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpJwtCliConfig {
    #[serde(default)]
    pub issuer: String,
    #[serde(default)]
    pub kid: String,
    #[serde(skip)]
    pub private_key_path: String,
}

impl McpJwtCliConfig {
    fn to_core(&self) -> Option<McpJwtConfig> {
        if self.issuer.is_empty() || self.kid.is_empty() || self.private_key_path.is_empty() {
            None
        } else {
            Some(McpJwtConfig {
                issuer: self.issuer.clone(),
                kid: self.kid.clone(),
                private_key_path: self.private_key_path.clone(),
            })
        }
    }
}

#[derive(Clone, Deserialize)]
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

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthConfig {
    #[serde(default)]
    pub github_redirect_uri: String,
    #[serde(default)]
    pub github_client_id: String,
    #[serde(skip)]
    pub github_client_secret: String,
    #[serde(default)]
    pub github_allowed_teams: Vec<String>,
}

#[derive(Clone, Default, Deserialize)]
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
}

impl Config {
    pub fn try_new(path: impl AsRef<Path>, secrets: EnvSecrets) -> anyhow::Result<Self> {
        let config_file = std::fs::read_to_string(&path).map_err(|e| {
            anyhow::anyhow!(
                "Couldn't read config file {:?}: {}",
                path.as_ref().display(),
                e
            )
        })?;

        let mut config: Config = serde_yaml::from_str(&config_file)
            .map_err(|e| anyhow::anyhow!("Invalid config: {e}"))?;

        config.db.pg_con = secrets.pg_con;
        config.oauth.github_client_secret = secrets.github_client_secret;
        // Trim to catch stray whitespace / trailing newline that often
        // sneaks in when the key was piped from `echo` or a broken
        // `.env`; both render the key invalid upstream for opaque reasons.
        config.anthropic_api_key = secrets.anthropic_api_key.trim().to_string();
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

        // MCP JWT signer private key path from env (K8s secret mount)
        if let Some(ref mut jwt) = config.mcp_jwt {
            if let Ok(val) = std::env::var("MCP_JWT_PRIVATE_KEY_PATH") {
                jwt.private_key_path = val;
            }
        }

        Ok(config)
    }

    pub fn mcp_jwt_config(&self) -> Option<McpJwtConfig> {
        self.mcp_jwt.as_ref().and_then(|c| c.to_core())
    }

    pub fn auth_config(&self) -> AuthConfig {
        AuthConfig {
            github_client_id: self.oauth.github_client_id.clone(),
            github_client_secret: self.oauth.github_client_secret.clone(),
            github_redirect_uri: self.oauth.github_redirect_uri.clone(),
            github_allowed_teams: self.oauth.github_allowed_teams.clone(),
        }
    }
}
