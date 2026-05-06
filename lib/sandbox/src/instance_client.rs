//! HTTP client for a single sandbox tool server. Wire types must mirror
//! `images/sandbox/server/src/main.rs`.

use opentelemetry::global;
use opentelemetry_http::HeaderInjector;
use serde::{Deserialize, Serialize};
use tracing::instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::error::InstanceError;
use crate::tool_protocol::{
    BashCommandInput, BashCommandOutput, DeleteInput, GlobInput, GrepInput, MoveInput,
    TextEditorInput,
};
use crate::types::{Sandbox, SandboxMode};

#[derive(Clone)]
pub struct InstanceClient {
    base_url: String,
    http: reqwest::Client,
}

/// Builds a `HeaderMap` carrying the W3C `traceparent` for the current
/// tracing span, so the sandbox-tool-server can attach to the same trace.
fn current_trace_headers() -> http::HeaderMap {
    let mut headers = http::HeaderMap::new();
    let cx = tracing::Span::current().context();
    global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&cx, &mut HeaderInjector(&mut headers));
    });
    headers
}

impl InstanceClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http: reqwest::Client::new(),
        }
    }

    /// Returns `None` if the sandbox isn't ready yet (no `base_url`).
    pub fn from_sandbox(sandbox: &Sandbox) -> Option<Self> {
        sandbox.base_url.as_ref().map(Self::new)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    #[instrument(name = "sandbox.instance.health", skip(self))]
    pub async fn health(&self) -> Result<(), InstanceError> {
        let resp = self
            .http
            .get(format!("{}/health", self.base_url))
            .headers(current_trace_headers())
            .send()
            .await?;
        resp.error_for_status()?;
        Ok(())
    }

    #[instrument(name = "sandbox.instance.initialize", skip_all)]
    pub async fn initialize(
        &self,
        req: &InitializeRequest,
    ) -> Result<InitializeResponse, InstanceError> {
        let resp = self
            .http
            .post(format!("{}/initialize", self.base_url))
            .headers(current_trace_headers())
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

    /// Notifies the sandbox-server that a new agent is attaching.
    /// The server resets per-tenant state — currently the persistent
    /// bash session's cwd — so the new agent doesn't inherit the
    /// prior tenant's `cd`. Idempotent.
    #[instrument(name = "sandbox.instance.attach", skip(self))]
    pub async fn attach(&self) -> Result<(), InstanceError> {
        self.http
            .post(format!("{}/attach", self.base_url))
            .headers(current_trace_headers())
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// Re-writes the GitHub App installation token at
    /// `/run/secrets/github-token` and `/workspace/.git-credentials`
    /// (and `/workspace/.gh-token`). GitHub installation tokens expire
    /// after 1h, so the orchestrator calls this on every workflow-run
    /// attach to keep `git push` and `gh` working on long-lived
    /// sandboxes.
    #[instrument(name = "sandbox.instance.refresh_credentials", skip_all)]
    pub async fn refresh_credentials(
        &self,
        req: &RefreshCredentialsRequest,
    ) -> Result<(), InstanceError> {
        let resp = self
            .http
            .post(format!("{}/refresh-credentials", self.base_url))
            .headers(current_trace_headers())
            .json(req)
            .send()
            .await?
            .error_for_status()?
            .json::<RefreshCredentialsResponse>()
            .await?;
        if let Some(err) = resp.error {
            return Err(InstanceError::Server(err));
        }
        Ok(())
    }

    /// Resets the cloned repo to a clean baseline (`git reset --hard
    /// origin/main` + drop `bot/*` branches). Best-effort: returns
    /// `Ok(ResetRepoResponse { ok: false, .. })` when individual steps
    /// fail (e.g. `git fetch` on a stale token); the caller decides
    /// whether to escalate.
    #[instrument(name = "sandbox.instance.reset_repo", skip(self))]
    pub async fn reset_repo(&self) -> Result<ResetRepoResponse, InstanceError> {
        let resp = self
            .http
            .post(format!("{}/reset-repo", self.base_url))
            .headers(current_trace_headers())
            .send()
            .await?
            .error_for_status()?
            .json::<ResetRepoResponse>()
            .await?;
        Ok(resp)
    }

    #[instrument(name = "sandbox.instance.execute", skip_all, fields(tool = %req.tool))]
    pub async fn execute(&self, req: &ExecuteRequest) -> Result<ExecuteResponse, InstanceError> {
        let resp = self
            .http
            .post(format!("{}/execute", self.base_url))
            .headers(current_trace_headers())
            .json(req)
            .send()
            .await?
            .error_for_status()?
            .json::<ExecuteResponse>()
            .await?;
        Ok(resp)
    }

    /// Typed POST to `/execute`. Skips the intermediate
    /// `serde_json::Value` that the loose-typed [`Self::execute`]
    /// path would force.
    async fn execute_typed<T: Serialize>(
        &self,
        tool: &str,
        input: &T,
    ) -> Result<ExecuteResponse, InstanceError> {
        let req = ExecuteRequestRef { tool, input };
        let resp = self
            .http
            .post(format!("{}/execute", self.base_url))
            .headers(current_trace_headers())
            .json(&req)
            .send()
            .await?
            .error_for_status()?
            .json::<ExecuteResponse>()
            .await?;
        Ok(resp)
    }

    /// Typed wrapper over `/execute` for the `bash` tool.
    #[instrument(name = "sandbox.instance.execute_bash", skip_all)]
    pub async fn execute_bash(
        &self,
        input: &BashCommandInput,
    ) -> Result<BashCommandOutput, InstanceError> {
        let resp = self.execute_typed("bash", input).await?;
        Ok(BashCommandOutput {
            output: resp.output,
            is_error: resp.is_error,
        })
    }

    /// Typed wrapper over `/execute` for the `Glob` tool.
    #[instrument(name = "sandbox.instance.execute_glob", skip_all)]
    pub async fn execute_glob(&self, input: &GlobInput) -> Result<ExecuteResponse, InstanceError> {
        self.execute_typed("Glob", input).await
    }

    /// Typed wrapper over `/execute` for the `Grep` tool.
    #[instrument(name = "sandbox.instance.execute_grep", skip_all)]
    pub async fn execute_grep(&self, input: &GrepInput) -> Result<ExecuteResponse, InstanceError> {
        self.execute_typed("Grep", input).await
    }

    /// Typed wrapper over `/execute` for the `Move` tool.
    #[instrument(name = "sandbox.instance.execute_move", skip_all)]
    pub async fn execute_move(&self, input: &MoveInput) -> Result<ExecuteResponse, InstanceError> {
        self.execute_typed("Move", input).await
    }

    /// Typed wrapper over `/execute` for the `Delete` tool.
    #[instrument(name = "sandbox.instance.execute_delete", skip_all)]
    pub async fn execute_delete(
        &self,
        input: &DeleteInput,
    ) -> Result<ExecuteResponse, InstanceError> {
        self.execute_typed("Delete", input).await
    }

    /// Typed wrapper over `/execute` for the `str_replace_based_edit_tool`.
    #[instrument(name = "sandbox.instance.execute_text_editor", skip_all)]
    pub async fn execute_text_editor(
        &self,
        input: &TextEditorInput,
    ) -> Result<ExecuteResponse, InstanceError> {
        self.execute_typed("str_replace_based_edit_tool", input)
            .await
    }
}

/// Borrowed mirror of [`ExecuteRequest`] used by typed wrappers like
/// [`InstanceClient::execute_bash`] to serialize directly to the wire
/// without going through an intermediate `serde_json::Value`.
#[derive(Serialize)]
struct ExecuteRequestRef<'a, T: Serialize> {
    tool: &'a str,
    input: &'a T,
}

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
    /// `github_token` is written to `GITHUB_TOKEN_PATH` inside the sandbox.
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
    /// From SKILL.md frontmatter, if present.
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

#[derive(Debug, Clone, Serialize)]
pub struct RefreshCredentialsRequest {
    pub github_token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RefreshCredentialsResponse {
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResetRepoResponse {
    pub ok: bool,
    #[serde(default)]
    pub output: String,
}
