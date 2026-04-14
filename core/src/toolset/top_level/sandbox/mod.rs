//! Sandbox tools — `bash` and `text_editor` — forwarded to a sandbox
//! tool server over HTTP.
//!
//! Each tool implements [`TopLevelTool`] and dispatches calls to
//! `POST {sandbox_url}/execute` with `{ tool, input }`.

mod bash;
mod text_editor;

pub use bash::SandboxBash;
pub use text_editor::SandboxTextEditor;

use serde::{Deserialize, Serialize};

use super::super::error::ToolSetsError;

/// Shared HTTP client for sandbox tool dispatch.
#[derive(Clone)]
pub(super) struct SandboxClient {
    http: reqwest::Client,
    base_url: String,
}

#[derive(Serialize)]
struct ToolRequest<'a> {
    tool: &'a str,
    input: &'a serde_json::Value,
}

#[derive(Deserialize)]
struct ToolResponse {
    output: String,
    is_error: bool,
}

impl SandboxClient {
    fn new(base_url: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url,
        }
    }

    /// Forward a tool call to the sandbox server and return `(output, is_error)`.
    async fn execute(
        &self,
        tool: &str,
        input: &serde_json::Value,
    ) -> Result<(String, bool), ToolSetsError> {
        let resp = self
            .http
            .post(format!("{}/execute", self.base_url))
            .json(&ToolRequest { tool, input })
            .send()
            .await
            .map_err(|e| ToolSetsError::SandboxHttp(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ToolSetsError::SandboxHttp(format!("HTTP {status}: {body}")));
        }

        let result: ToolResponse = resp
            .json()
            .await
            .map_err(|e| ToolSetsError::SandboxHttp(e.to_string()))?;

        Ok((result.output, result.is_error))
    }
}
