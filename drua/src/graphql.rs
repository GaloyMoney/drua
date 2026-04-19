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
