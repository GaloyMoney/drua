use std::io::{self, Write};

use anyhow::{anyhow, Result};
use serde::Deserialize;

use crate::config::Config;
use crate::graphql::GraphqlClient;

const DEFAULT_SERVER_URL: &str = "http://localhost:4200";

#[derive(Debug, Deserialize)]
struct MeResponse {
    me: Option<MeUser>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MeUser {
    github_username: Option<String>,
    name: Option<String>,
    email: Option<String>,
}

pub async fn run(server: Option<String>) -> Result<()> {
    let server_url = server
        .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string())
        .trim_end_matches('/')
        .to_string();

    let creds_url = format!("{server_url}/auth/cli-login");
    println!("Opening browser to create an API token...");
    println!("  {creds_url}");
    println!();
    println!("Log in if prompted — a token will be generated automatically.");
    println!();

    let _ = open_browser(&creds_url);

    print!("Paste your API token: ");
    io::stdout().flush()?;
    let mut token = String::new();
    io::stdin().read_line(&mut token)?;
    let token = token.trim().to_string();

    if token.is_empty() {
        return Err(anyhow!("no token provided"));
    }

    println!("Validating token...");
    let client = GraphqlClient::new(&server_url, &token);
    let resp: MeResponse = client
        .query(
            "{ me { githubUsername name email } }",
            serde_json::json!({}),
        )
        .await
        .map_err(|e| anyhow!("authentication failed: {e}"))?;

    let user = resp.me.ok_or_else(|| anyhow!("token is not valid"))?;
    let display = user
        .github_username
        .or(user.name)
        .or(user.email)
        .unwrap_or_else(|| "unknown".to_string());

    let config = Config {
        server_url,
        auth_token: token,
        last_project_id: None,
    };
    config.save()?;

    println!("Authenticated as {display}");
    Ok(())
}

fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", url])
            .spawn()?;
    }
    Ok(())
}
