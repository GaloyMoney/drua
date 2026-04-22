use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub server_url: String,
    pub auth_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_agent_id: Option<String>,
}

fn config_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(home.join(".drua"))
}

fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.json"))
}

pub const DEFAULT_SERVER_URL: &str = "http://localhost:4200";

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        let contents =
            fs::read_to_string(&path).context("not logged in — run `drua login` first")?;
        let config: Config = serde_json::from_str(&contents).context("invalid config file")?;
        Ok(config)
    }

    /// Load config, or auto-authenticate via the dev-token endpoint if the
    /// server is running with `oauth.login=dev`.
    pub async fn load_or_dev_login(server: Option<String>) -> Result<Self> {
        if let Ok(config) = Self::load() {
            return Ok(config);
        }

        let server_url = server
            .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string())
            .trim_end_matches('/')
            .to_string();

        let http = reqwest::Client::new();
        let resp = http
            .post(format!("{server_url}/auth/dev-token"))
            .send()
            .await;

        let resp = match resp {
            Ok(r) if r.status().is_success() => r,
            _ => anyhow::bail!("not logged in — run `drua login` first"),
        };

        let body: serde_json::Value = resp.json().await?;
        let token = body
            .get("token")
            .and_then(|t| t.as_str())
            .context("unexpected response from dev-token endpoint")?;

        let config = Config {
            server_url,
            auth_token: token.to_string(),
            chat_agent_id: None,
        };
        config.save()?;
        eprintln!("Auto-authenticated as Dev User");
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        let dir = config_dir()?;
        fs::create_dir_all(&dir).context("failed to create config directory")?;

        let path = config_path()?;
        let contents = serde_json::to_string_pretty(self)?;
        fs::write(&path, contents).context("failed to write config file")?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(0o600);
            fs::set_permissions(&path, perms).context("failed to set config permissions")?;
        }

        Ok(())
    }

    pub fn delete() -> Result<()> {
        let path = config_path()?;
        if path.exists() {
            fs::remove_file(&path).context("failed to delete config file")?;
        }
        Ok(())
    }
}
