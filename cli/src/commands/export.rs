use anyhow::Result;
use serde::Deserialize;

use crate::config::Config;
use crate::graphql::GraphqlClient;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportThreadResponse {
    export_thread: String,
}

/// Export the current thread for an agent as Pi-compatible JSONL and print to stdout.
pub async fn run(agent_id: &str) -> Result<()> {
    let config = Config::load()?;
    let client = GraphqlClient::new(&config.server_url, &config.auth_token);

    let query = r#"
        query ExportThread($agentId: AgentId!) {
            exportThread(agentId: $agentId)
        }
    "#;

    let resp: ExportThreadResponse = client
        .query(query, serde_json::json!({ "agentId": agent_id }))
        .await?;

    print!("{}", resp.export_thread);
    Ok(())
}
