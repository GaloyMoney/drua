//! HTTP client for a single running sandbox instance.
//!
//! Wraps the sandbox tool server's `/initialize` and `/execute` endpoints.
//! Wire types here mirror the request/response shapes defined in
//! `images/sandbox/server/src/main.rs` — keep the two in sync.

use serde::{Deserialize, Serialize};
use tracing::instrument;

use crate::error::InstanceError;
use crate::types::{Sandbox, SandboxMode};

/// HTTP client bound to a single sandbox base URL.
#[derive(Clone)]
pub struct InstanceClient {
    base_url: String,
    http: reqwest::Client,
}

impl InstanceClient {
    /// Build a client targeting `base_url` (e.g. `http://127.0.0.1:34567`).
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http: reqwest::Client::new(),
        }
    }

    /// Build a client from a [`Sandbox`] handle, returning `None` if the
    /// sandbox isn't ready yet (no `base_url` populated).
    pub fn from_sandbox(sandbox: &Sandbox) -> Option<Self> {
        sandbox.base_url.as_ref().map(Self::new)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `GET /health` — returns Ok if the server responds with 2xx.
    #[instrument(name = "sandbox.instance.health", skip(self))]
    pub async fn health(&self) -> Result<(), InstanceError> {
        let resp = self
            .http
            .get(format!("{}/health", self.base_url))
            .send()
            .await?;
        resp.error_for_status()?;
        Ok(())
    }

    /// `POST /initialize` — set up the sandbox workspace (scratch or repo
    /// mode) and optionally write the GitHub token.
    #[instrument(name = "sandbox.instance.initialize", skip_all)]
    pub async fn initialize(
        &self,
        req: &InitializeRequest,
    ) -> Result<InitializeResponse, InstanceError> {
        let resp = self
            .http
            .post(format!("{}/initialize", self.base_url))
            .json(req)
            .send()
            .await?
            .error_for_status()?
            .json::<InitializeResponse>()
            .await?;

        if let Some(err) = &resp.error {
            return Err(InstanceError::Server(err.clone()));
        }
        Ok(resp)
    }

    /// `POST /execute` — invoke a tool inside the sandbox.
    #[instrument(name = "sandbox.instance.execute", skip_all, fields(tool = %req.tool))]
    pub async fn execute(&self, req: &ExecuteRequest) -> Result<ExecuteResponse, InstanceError> {
        let resp = self
            .http
            .post(format!("{}/execute", self.base_url))
            .json(req)
            .send()
            .await?
            .error_for_status()?
            .json::<ExecuteResponse>()
            .await?;
        Ok(resp)
    }
}

// ---------------------------------------------------------------------------
// Wire types — mirror images/sandbox/server/src/main.rs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct InitializeRequest {
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub github_token: Option<String>,
}

impl InitializeRequest {
    /// Build a request from a [`SandboxMode`], optionally including a
    /// GitHub token to be written to `GITHUB_TOKEN_PATH` inside the sandbox.
    pub fn from_mode(mode: &SandboxMode, github_token: Option<String>) -> Self {
        match mode {
            SandboxMode::Scratch => Self {
                mode: "scratch".to_string(),
                repo_url: None,
                branch: None,
                github_token,
            },
            SandboxMode::Repo { repo_url, branch } => Self {
                mode: "repo".to_string(),
                repo_url: Some(repo_url.clone()),
                branch: branch.clone(),
                github_token,
            },
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct InitializeResponse {
    pub cwd: String,
    #[serde(default)]
    pub exported_system_prompt: Option<ExportedFile>,
    #[serde(default)]
    pub exported_skills: Vec<ExportedSkill>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExportedFile {
    pub file_name: String,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExportedSkill {
    pub name: String,
    pub content: String,
    /// Short description extracted from SKILL.md frontmatter, if present.
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecuteRequest {
    pub tool: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExecuteResponse {
    pub output: String,
    pub is_error: bool,
}
