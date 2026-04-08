use std::path::Path;

use serde::Deserialize;

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
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub toolsets: ToolSetsConfig,
    #[serde(skip)]
    pub anthropic_api_key: String,
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    /// Enable Agent Sandbox integration.
    #[serde(default)]
    pub enabled: bool,

    /// Kubernetes namespace where sandboxes are managed.
    #[serde(default)]
    pub namespace: String,

    /// Name of the SandboxTemplate to use when creating claims.
    #[serde(default)]
    pub template_name: String,

    /// Persistent storage configuration for sandbox workspaces.
    #[serde(default)]
    pub persistence: Option<SandboxPersistenceConfig>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxPersistenceConfig {
    /// PVC size (e.g., "10Gi").
    pub size: String,
    /// StorageClass name (e.g., "pd-balanced").
    pub storage_class: String,
    /// Mount path inside the container (e.g., "/workspace").
    pub mount_path: String,
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
        config.anthropic_api_key = secrets.anthropic_api_key;
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

        Ok(config)
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
