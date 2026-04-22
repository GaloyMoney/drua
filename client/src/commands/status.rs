use anyhow::{anyhow, Result};
use serde::Deserialize;

use crate::config::Config;
use crate::graphql::GraphqlClient;

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

pub async fn run() -> Result<()> {
    let config = Config::load_or_dev_login(None).await?;
    let client = GraphqlClient::new(&config.server_url, &config.auth_token);

    let resp: MeResponse = client
        .query(
            "{ me { githubUsername name email } }",
            serde_json::json!({}),
        )
        .await?;

    match resp.me {
        Some(user) => {
            let display = user
                .github_username
                .or(user.name)
                .or(user.email)
                .unwrap_or_else(|| "unknown".to_string());
            println!("Server:  {}", config.server_url);
            println!("User:    {display}");
            println!("Status:  connected");
        }
        None => {
            return Err(anyhow!(
                "token is no longer valid — run `drua login` to re-authenticate"
            ));
        }
    }

    Ok(())
}
