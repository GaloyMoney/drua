use anyhow::{anyhow, Result};
use futures::{SinkExt, StreamExt};
use serde::{de::DeserializeOwned, Deserialize};
use tokio_tungstenite::tungstenite;

#[derive(Debug, Deserialize)]
struct GraphqlResponse<T> {
    data: Option<T>,
    errors: Option<Vec<GraphqlError>>,
}

#[derive(Debug, Deserialize)]
struct GraphqlError {
    message: String,
}

impl std::fmt::Display for GraphqlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

pub struct GraphqlClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl GraphqlClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into(),
            token: token.into(),
        }
    }

    pub async fn query<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<T> {
        let body = serde_json::json!({
            "query": query,
            "variables": variables,
        });

        let resp = self
            .http
            .post(format!("{}/graphql", self.base_url))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!("request failed: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("server returned {status}: {text}"));
        }

        let gql_resp: GraphqlResponse<T> = resp
            .json()
            .await
            .map_err(|e| anyhow!("failed to parse response: {e}"))?;

        if let Some(errors) = gql_resp.errors {
            let msgs: Vec<String> = errors.iter().map(|e| e.message.clone()).collect();
            return Err(anyhow!("GraphQL errors: {}", msgs.join(", ")));
        }

        gql_resp.data.ok_or_else(|| anyhow!("no data in response"))
    }
}

// ---------------------------------------------------------------------------
// GraphQL subscription over WebSocket (graphql-transport-ws protocol)
// ---------------------------------------------------------------------------

const AGENT_MESSAGE_SUBSCRIPTION: &str = r#"
subscription AgentMessage($agentId: AgentId!, $prompt: String!) {
  agentSendMessage(agentId: $agentId, prompt: $prompt) {
    __typename
    ... on TextDeltaEvent { text }
    ... on ThinkingDeltaEvent { text }
    ... on ToolCallStartEvent { name }
    ... on ToolCallInputDeltaEvent { partialJson }
    ... on AssistantTextEvent { text }
    ... on ThinkingEvent { text }
    ... on UserMessageEvent { text }
    ... on ToolCallEvent { name arguments }
    ... on ToolResultEvent { name isError }
    ... on AssistantDoneEvent { turns inputTokens outputTokens durationMs costUsd }
    ... on ErrorEvent { message }
    ... on ServiceEvent { message }
  }
}
"#;

/// Stream agent message events over a GraphQL subscription (WebSocket).
///
/// Each received event is parsed and sent to the `on_event` callback.
/// Returns when the subscription completes or an error occurs.
pub async fn subscribe_agent_message<F>(
    base_url: &str,
    token: &str,
    agent_id: &str,
    prompt: &str,
    on_event: F,
) -> Result<()>
where
    F: Fn(serde_json::Value) -> bool,
{
    let ws_url = base_url
        .replace("http://", "ws://")
        .replace("https://", "wss://");
    let ws_url = format!("{ws_url}/graphql/ws");

    let mut request = tungstenite::client::IntoClientRequest::into_client_request(ws_url)?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse().unwrap());
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        "graphql-transport-ws".parse().unwrap(),
    );

    let (mut ws, _) = tokio_tungstenite::connect_async(request).await?;

    // connection_init
    ws.send(tungstenite::Message::Text(
        serde_json::json!({"type": "connection_init"})
            .to_string()
            .into(),
    ))
    .await?;

    // Wait for connection_ack
    loop {
        let msg = ws
            .next()
            .await
            .ok_or_else(|| anyhow!("WebSocket closed before connection_ack"))??;
        if let tungstenite::Message::Text(text) = msg {
            let parsed: serde_json::Value = serde_json::from_str(&text)?;
            if parsed["type"] == "connection_ack" {
                break;
            }
        }
    }

    // Subscribe
    let subscribe_msg = serde_json::json!({
        "type": "subscribe",
        "id": "1",
        "payload": {
            "query": AGENT_MESSAGE_SUBSCRIPTION,
            "variables": {
                "agentId": agent_id,
                "prompt": prompt,
            }
        }
    });
    ws.send(tungstenite::Message::Text(subscribe_msg.to_string().into()))
        .await?;

    // Receive events
    while let Some(msg) = ws.next().await {
        let msg = msg?;
        match msg {
            tungstenite::Message::Text(text) => {
                let parsed: serde_json::Value = serde_json::from_str(&text)?;
                match parsed["type"].as_str() {
                    Some("next") => {
                        if let Some(event) = parsed
                            .get("payload")
                            .and_then(|p| p.get("data"))
                            .and_then(|d| d.get("agentSendMessage"))
                        {
                            if !on_event(event.clone()) {
                                break;
                            }
                        }
                    }
                    Some("error") => {
                        let msg = parsed
                            .get("payload")
                            .map(|p| p.to_string())
                            .unwrap_or_else(|| "unknown error".to_string());
                        return Err(anyhow!("Subscription error: {msg}"));
                    }
                    Some("complete") => break,
                    _ => {}
                }
            }
            tungstenite::Message::Close(_) => break,
            _ => {}
        }
    }

    Ok(())
}
