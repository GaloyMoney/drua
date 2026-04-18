use std::io::{self, Write};

use anyhow::{anyhow, Result};
use serde::Deserialize;

use crate::config::Config;

const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const GITHUB_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const GITHUB_SCOPES: &str = "read:user user:email read:org";
const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct CliTokenResponse {
    token: String,
    user_id: String,
}

#[derive(Debug, Deserialize)]
struct CliTokenError {
    error: String,
}

pub async fn run(server: Option<String>, github_client_id: Option<String>) -> Result<()> {
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

    let client_id = match github_client_id {
        Some(id) => id,
        None => {
            print!("GitHub OAuth Client ID: ");
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            input.trim().to_string()
        }
    };

    if client_id.is_empty() {
        return Err(anyhow!(
            "GitHub client ID is required — pass --github-client-id or set DRUA_GITHUB_CLIENT_ID"
        ));
    }

    // Step 1: Request device code from GitHub
    let http = reqwest::Client::new();
    let device_resp = http
        .post(GITHUB_DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .form(&[("client_id", client_id.as_str()), ("scope", GITHUB_SCOPES)])
        .send()
        .await
        .map_err(|e| anyhow!("failed to request device code: {e}"))?;

    if !device_resp.status().is_success() {
        let text = device_resp.text().await.unwrap_or_default();
        return Err(anyhow!("GitHub device code request failed: {text}"));
    }

    let device: DeviceCodeResponse = device_resp
        .json()
        .await
        .map_err(|e| anyhow!("failed to parse device code response: {e}"))?;

    // Step 2: Display user code and open browser
    println!();
    println!("  Open: {}", device.verification_uri);
    println!("  Enter code: {}", device.user_code);
    println!();

    let _ = open_browser(&device.verification_uri);

    println!(
        "Waiting for authorization (expires in {}s)...",
        device.expires_in
    );

    // Step 3: Poll for GitHub access token
    let mut interval = device.interval;
    let github_token = loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;

        let resp = http
            .post(GITHUB_TOKEN_URL)
            .header("Accept", "application/json")
            .form(&[
                ("client_id", client_id.as_str()),
                ("device_code", device.device_code.as_str()),
                ("grant_type", DEVICE_CODE_GRANT_TYPE),
            ])
            .send()
            .await
            .map_err(|e| anyhow!("token poll failed: {e}"))?;

        let token_resp: TokenResponse = resp
            .json()
            .await
            .map_err(|e| anyhow!("failed to parse token response: {e}"))?;

        if let Some(access_token) = token_resp.access_token {
            break access_token;
        }

        match token_resp.error.as_deref() {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                interval = token_resp.interval.unwrap_or(interval + 5);
                continue;
            }
            Some("expired_token") => return Err(anyhow!("device code expired — please try again")),
            Some("access_denied") => return Err(anyhow!("authorization was denied by the user")),
            Some(other) => return Err(anyhow!("GitHub OAuth error: {other}")),
            None => return Err(anyhow!("unexpected response from GitHub")),
        }
    };

    // Step 4: Exchange GitHub token with server for a CLI bearer token
    println!("Exchanging token with server...");
    let exchange_resp = http
        .post(format!("{server_url}/auth/cli/token"))
        .bearer_auth(&github_token)
        .send()
        .await
        .map_err(|e| anyhow!("server token exchange failed: {e}"))?;

    if !exchange_resp.status().is_success() {
        let err: CliTokenError = exchange_resp.json().await.unwrap_or(CliTokenError {
            error: "unknown server error".to_string(),
        });
        return Err(anyhow!("login failed: {}", err.error));
    }

    let cli_token: CliTokenResponse = exchange_resp
        .json()
        .await
        .map_err(|e| anyhow!("failed to parse server response: {e}"))?;

    // Step 5: Save config
    let config = Config {
        server_url,
        auth_token: cli_token.token,
    };
    config.save()?;

    println!("Authenticated as {}", cli_token.user_id);
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
