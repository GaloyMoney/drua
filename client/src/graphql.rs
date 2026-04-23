use anyhow::{anyhow, Result};
use serde::{de::DeserializeOwned, Deserialize};

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
// GraphQL subscription over SSE (POST /graphql/stream)
// ---------------------------------------------------------------------------

const AGENT_MESSAGE_SUBSCRIPTION: &str = r#"
subscription AgentMessage($agentId: AgentId!, $prompt: String!) {
  agentSendMessage(agentId: $agentId, prompt: $prompt) {
    __typename
    ... on AssistantTextDeltaEvent { text }
    ... on ThinkingDeltaEvent { text }
    ... on ToolCallStartEvent { name }
    ... on ToolCallInputDeltaEvent { partialJson }
    ... on AssistantTextEvent { text }
    ... on ThinkingEvent { text }
    ... on UserMessageEvent { text }
    ... on ToolCallEvent { name arguments }
    ... on ToolResultEvent { name isError content }
    ... on AssistantDoneEvent { turns inputTokens outputTokens cacheReadInputTokens cacheCreationInputTokens durationMs costUsd }
    ... on ErrorEvent { message }
    ... on ServiceEvent { message }
  }
}
"#;

/// Stream agent message events over a GraphQL subscription (SSE).
///
/// POSTs the subscription query to `/graphql/stream` and parses the SSE
/// event stream. Each `next` event carries a GraphQL response; the
/// `complete` event signals end of stream.
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
    let http = reqwest::Client::new();
    let url = format!("{base_url}/graphql/stream");

    let body = serde_json::json!({
        "query": AGENT_MESSAGE_SUBSCRIPTION,
        "variables": {
            "agentId": agent_id,
            "prompt": prompt,
        }
    });

    let resp = http
        .post(&url)
        .bearer_auth(token)
        .header("Accept", "text/event-stream")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("server returned {status}: {text}");
    }

    let mut buf = String::new();
    let mut response = resp;

    while let Some(chunk) = response.chunk().await? {
        let text = String::from_utf8_lossy(&chunk);
        buf.push_str(&text);

        while let Some(end) = buf.find("\n\n") {
            let block = buf[..end].to_string();
            buf = buf[end + 2..].to_string();

            let mut event_type = String::new();
            let mut data = String::new();
            for line in block.lines() {
                if let Some(rest) = line.strip_prefix("event:") {
                    event_type = rest.trim().to_string();
                } else if let Some(rest) = line.strip_prefix("data:") {
                    data = rest.trim().to_string();
                }
            }

            if event_type == "complete" {
                return Ok(());
            }

            if event_type == "next" && !data.is_empty() {
                let gql_resp: serde_json::Value = serde_json::from_str(&data)?;

                // Check for GraphQL errors
                if let Some(errors) = gql_resp.get("errors").and_then(|e| e.as_array()) {
                    if let Some(first) = errors.first() {
                        let msg = first
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("unknown error");
                        return Err(anyhow!("GraphQL error: {msg}"));
                    }
                }

                if let Some(event) = gql_resp.get("data").and_then(|d| d.get("agentSendMessage")) {
                    if !on_event(event.clone()) {
                        return Ok(());
                    }
                }
            }
        }
    }

    Ok(())
}
