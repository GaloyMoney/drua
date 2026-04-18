use std::io::{self, Write};

use anyhow::{anyhow, Result};
use serde::Deserialize;

use crate::config::Config;
use crate::graphql::GraphqlClient;

#[derive(Debug, Deserialize)]
struct MeResponse {
    me: Option<String>,
}

pub async fn run(server: Option<String>) -> Result<()> {
    let server_url = match server {
        Some(url) => url,
        None => {
            print!("Server URL: ");
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            input.trim().to_string()
        }
    };

    let server_url = server_url.trim_end_matches('/').to_string();

    let creds_url = format!("{server_url}/dashboard/mcp-creds");
    println!("Opening browser to create an API token...");
    println!("  {creds_url}");
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
        .query("{ me }", serde_json::json!({}))
        .await
        .map_err(|e| anyhow!("authentication failed: {e}"))?;

    let user_id = resp.me.ok_or_else(|| anyhow!("token is not valid"))?;

    let config = Config {
        server_url,
        auth_token: token,
    };
    config.save()?;

    println!("Authenticated as {user_id}");
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
