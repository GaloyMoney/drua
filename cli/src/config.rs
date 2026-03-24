use std::path::Path;

use serde::Deserialize;

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
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_host")]
    pub host: String,
}

fn default_port() -> u16 {
    4200
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: default_port(),
            host: default_host(),
        }
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthConfig {
    #[serde(default)]
    pub github_redirect_uri: String,
    #[serde(skip)]
    pub github_client_id: String,
    #[serde(skip)]
    pub github_client_secret: String,
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbConfig {
    #[serde(skip)]
    pub pg_con: String,
}

pub struct EnvSecrets {
    pub pg_con: String,
    pub github_client_id: String,
    pub github_client_secret: String,
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
        config.oauth.github_client_id = secrets.github_client_id;
        config.oauth.github_client_secret = secrets.github_client_secret;

        Ok(config)
    }

    pub fn auth_config(&self) -> AuthConfig {
        AuthConfig {
            github_client_id: self.oauth.github_client_id.clone(),
            github_client_secret: self.oauth.github_client_secret.clone(),
            github_redirect_uri: self.oauth.github_redirect_uri.clone(),
        }
    }
}
