use anyhow::{anyhow, Result};
use serde::Deserialize;

use crate::config::Config;
use crate::graphql::GraphqlClient;

#[derive(Debug, Deserialize)]
struct MeResponse {
    me: Option<String>,
}

pub async fn run() -> Result<()> {
    let config = Config::load()?;
    let client = GraphqlClient::new(&config.server_url, &config.auth_token);

    let resp: MeResponse = client.query("{ me }", serde_json::json!({})).await?;

    match resp.me {
        Some(user_id) => {
            println!("Server:  {}", config.server_url);
            println!("User:    {user_id}");
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
