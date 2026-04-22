use url::Url;

use crate::auth::AuthManager;
use crate::error::ConcourseError;
use crate::types::*;

/// Async client for the Concourse CI REST API.
///
/// All methods take `&self` — interior mutability for the token cache
/// is handled by `AuthManager` using `tokio::sync::RwLock`.
pub struct ConcourseClient {
    http: reqwest::Client,
    base_url: Url,
    team: String,
    auth: AuthManager,
}

impl ConcourseClient {
    /// Create a new client.
    ///
    /// * `base_url` — root URL of the Concourse instance (e.g. `https://ci.example.com`)
    /// * `team` — default team name for API calls
    /// * `username` / `password` — credentials for sky token exchange
    pub fn new(
        base_url: &str,
        team: String,
        username: String,
        password: String,
    ) -> Result<Self, ConcourseError> {
        let base_url = Url::parse(base_url)?;
        let http = reqwest::Client::builder()
            .user_agent("concourse-client-rs/0.1")
            .build()?;
        Ok(Self {
            http,
            base_url,
            team,
            auth: AuthManager::new(username, password),
        })
    }

    // ----- helpers ----------------------------------------------------------

    fn api_url(&self, path: &str) -> Result<Url, ConcourseError> {
        let url = self.base_url.join(&format!("/api/v1{path}"))?;
        Ok(url)
    }

    /// GET with auth, automatic 401 retry.
    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, ConcourseError> {
        self.auth.ensure_token(&self.http, &self.base_url).await?;
        let url = self.api_url(path)?;

        let resp = self
            .http
            .get(url.clone())
            .headers(self.auth.auth_headers().await)
            .send()
            .await?;

        if resp.status().as_u16() == 401 {
            self.auth.clear_token().await;
            self.auth.ensure_token(&self.http, &self.base_url).await?;
            let resp = self
                .http
                .get(url)
                .headers(self.auth.auth_headers().await)
                .send()
                .await?;
            return Self::handle_response(resp).await;
        }

        Self::handle_response(resp).await
    }

    /// POST (empty body) with auth, automatic 401 retry.
    async fn post_empty<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, ConcourseError> {
        self.auth.ensure_token(&self.http, &self.base_url).await?;
        let url = self.api_url(path)?;

        let resp = self
            .http
            .post(url.clone())
            .headers(self.auth.auth_headers().await)
            .send()
            .await?;

        if resp.status().as_u16() == 401 {
            self.auth.clear_token().await;
            self.auth.ensure_token(&self.http, &self.base_url).await?;
            let resp = self
                .http
                .post(url)
                .headers(self.auth.auth_headers().await)
                .send()
                .await?;
            return Self::handle_response(resp).await;
        }

        Self::handle_response(resp).await
    }

    async fn handle_response<T: serde::de::DeserializeOwned>(
        resp: reqwest::Response,
    ) -> Result<T, ConcourseError> {
        let status = resp.status();
        if !status.is_success() {
            let code = status.as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ConcourseError::Api {
                status: code,
                message: body,
            });
        }
        let body = resp.json::<T>().await?;
        Ok(body)
    }

    // ----- public API -------------------------------------------------------

    /// Return the configured team name.
    pub fn team(&self) -> &str {
        &self.team
    }

    /// `GET /api/v1/pipelines` — list all pipelines across all accessible teams.
    #[tracing::instrument(name = "concourse_client.list_all_pipelines", skip_all)]
    pub async fn list_all_pipelines(&self) -> Result<Vec<Pipeline>, ConcourseError> {
        self.get("/pipelines").await
    }

    /// `GET /api/v1/teams/{team}/pipelines` — list pipelines for a specific team.
    #[tracing::instrument(name = "concourse_client.list_pipelines", skip_all)]
    pub async fn list_pipelines(&self) -> Result<Vec<Pipeline>, ConcourseError> {
        let team = &self.team;
        self.get(&format!("/teams/{team}/pipelines")).await
    }

    /// Resolve the team that owns a pipeline by searching all accessible pipelines.
    ///
    /// Returns the team name, or falls back to the default team if not found.
    #[tracing::instrument(name = "concourse_client.resolve_pipeline_team", skip_all)]
    pub async fn resolve_pipeline_team(&self, pipeline: &str) -> Result<String, ConcourseError> {
        let pipelines = self.list_all_pipelines().await?;
        if let Some(p) = pipelines.iter().find(|p| p.name == pipeline) {
            Ok(p.team_name.clone())
        } else {
            Ok(self.team.clone())
        }
    }

    /// `GET /api/v1/teams/{team}/pipelines/{pipeline}/jobs` — list jobs in a pipeline.
    ///
    /// Automatically resolves the team from the pipeline name.
    #[tracing::instrument(name = "concourse_client.list_jobs", skip_all)]
    pub async fn list_jobs(&self, pipeline: &str) -> Result<Vec<Job>, ConcourseError> {
        let team = self.resolve_pipeline_team(pipeline).await?;
        self.get(&format!("/teams/{team}/pipelines/{pipeline}/jobs"))
            .await
    }

    /// `GET /api/v1/teams/{team}/pipelines/{pipeline}/jobs/{job}/builds` —
    /// list builds for a job (most recent first).
    ///
    /// Automatically resolves the team from the pipeline name.
    #[tracing::instrument(name = "concourse_client.list_job_builds", skip_all)]
    pub async fn list_job_builds(
        &self,
        pipeline: &str,
        job: &str,
        limit: Option<usize>,
    ) -> Result<Vec<Build>, ConcourseError> {
        let team = self.resolve_pipeline_team(pipeline).await?;
        let query = match limit {
            Some(n) => format!("?limit={n}"),
            None => String::new(),
        };
        self.get(&format!(
            "/teams/{team}/pipelines/{pipeline}/jobs/{job}/builds{query}"
        ))
        .await
    }

    /// `GET /api/v1/builds/{id}` — get build details.
    #[tracing::instrument(name = "concourse_client.get_build", skip_all)]
    pub async fn get_build(&self, build_id: i64) -> Result<Build, ConcourseError> {
        self.get(&format!("/builds/{build_id}")).await
    }

    /// `GET /api/v1/teams/{team}/pipelines/{pipeline}/config` — get pipeline configuration.
    #[tracing::instrument(name = "concourse_client.get_pipeline_config", skip_all)]
    pub async fn get_pipeline_config(
        &self,
        pipeline: &str,
    ) -> Result<PipelineConfigResponse, ConcourseError> {
        let team = self.resolve_pipeline_team(pipeline).await?;
        self.get(&format!("/teams/{team}/pipelines/{pipeline}/config"))
            .await
    }

    /// `GET /api/v1/builds/{id}/resources` — get resource inputs/outputs for a build.
    #[tracing::instrument(name = "concourse_client.get_build_resources", skip_all)]
    pub async fn get_build_resources(
        &self,
        build_id: i64,
    ) -> Result<BuildResources, ConcourseError> {
        self.get(&format!("/builds/{build_id}/resources")).await
    }

    /// `GET /api/v1/builds/{id}/events` — fetch build log output.
    ///
    /// Streams the SSE event stream and extracts log lines from `"log"` events.
    /// Stops when an `"end"` event is received or after a 30-second read timeout.
    #[tracing::instrument(name = "concourse_client.get_build_logs", skip_all)]
    pub async fn get_build_logs(&self, build_id: i64) -> Result<String, ConcourseError> {
        use futures_util::StreamExt;
        use std::time::Duration;

        let resp = self.open_build_events(build_id).await?;
        let mut stream = resp.bytes_stream();
        let mut body = String::new();

        // Read chunks with a per-chunk timeout; SSE streams don't close naturally
        loop {
            match tokio::time::timeout(Duration::from_secs(30), stream.next()).await {
                Ok(Some(Ok(chunk))) => {
                    body.push_str(&String::from_utf8_lossy(&chunk));
                    // Stop early if we see the end event
                    if body.contains("\"event\":\"end\"") {
                        break;
                    }
                }
                Ok(Some(Err(e))) => {
                    tracing::warn!(error = %e, "Error reading build event stream");
                    break;
                }
                Ok(None) => break, // Stream ended
                Err(_) => break,   // Timeout — return what we have
            }
        }

        let mut logs = String::new();
        extract_logs_from_sse(&body, &mut logs);
        Ok(logs)
    }

    /// Open an authenticated SSE connection to build events.
    ///
    /// Returns the raw response for streaming consumption by [`BuildLogStore`].
    pub(crate) async fn open_build_events(
        &self,
        build_id: i64,
    ) -> Result<reqwest::Response, ConcourseError> {
        self.auth.ensure_token(&self.http, &self.base_url).await?;
        let url = self.api_url(&format!("/builds/{build_id}/events"))?;

        let resp = self
            .http
            .get(url)
            .headers(self.auth.auth_headers().await)
            .header("Accept", "text/event-stream")
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ConcourseError::Api {
                status,
                message: body,
            });
        }

        Ok(resp)
    }

    /// `POST /api/v1/teams/{team}/pipelines/{pipeline}/jobs/{job}/builds` —
    /// trigger a new build.
    #[tracing::instrument(name = "concourse_client.trigger_build", skip_all)]
    pub async fn trigger_build(&self, pipeline: &str, job: &str) -> Result<Build, ConcourseError> {
        let team = self.resolve_pipeline_team(pipeline).await?;
        self.post_empty(&format!(
            "/teams/{team}/pipelines/{pipeline}/jobs/{job}/builds"
        ))
        .await
    }
}

/// Format a Unix epoch timestamp as `HH:MM:SS` UTC.
pub(crate) fn format_epoch(epoch: i64) -> String {
    let secs = ((epoch % 86400) + 86400) % 86400; // handle negative mod
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

/// Parse SSE text and extract log lines from event envelopes.
fn extract_logs_from_sse(body: &str, out: &mut String) {
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        let Ok(envelope) = serde_json::from_str::<BuildEventEnvelope>(data) else {
            continue;
        };
        if envelope.event != "log" {
            continue;
        }
        if let Some(payload) = envelope.data.get("payload") {
            if let Some(text) = payload.as_str() {
                // Prepend timestamp if available in the event data
                if let Some(ts) = envelope.data.get("time").and_then(|v| v.as_i64()) {
                    let stamp = format_epoch(ts);
                    // Prefix each non-empty line with the timestamp
                    for (i, line) in text.split('\n').enumerate() {
                        if i > 0 {
                            out.push('\n');
                        }
                        if !line.is_empty() {
                            out.push_str(&format!("[{stamp}] {line}"));
                        }
                    }
                } else {
                    out.push_str(text);
                }
            }
        }
    }
}
