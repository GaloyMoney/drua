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

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        let contents =
            fs::read_to_string(&path).context("not logged in — run `drua login` first")?;
        let config: Config = serde_json::from_str(&contents).context("invalid config file")?;
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
