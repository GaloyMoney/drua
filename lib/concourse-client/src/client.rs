use url::Url;

use crate::auth::AuthManager;
use crate::error::ConcourseError;
use crate::types::*;

/// Async client for the Concourse CI REST API. All methods take `&self` —
/// `AuthManager` handles token-cache interior mutability via `RwLock`.
pub struct ConcourseClient {
    http: reqwest::Client,
    base_url: Url,
    team: String,
    auth: AuthManager,
}

impl ConcourseClient {
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
            return Self::handle_response("GET", path, resp).await;
        }

        Self::handle_response("GET", path, resp).await
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
            return Self::handle_response("POST", path, resp).await;
        }

        Self::handle_response("POST", path, resp).await
    }

    async fn handle_response<T: serde::de::DeserializeOwned>(
        method: &str,
        path: &str,
        resp: reqwest::Response,
    ) -> Result<T, ConcourseError> {
        let status = resp.status();
        if !status.is_success() {
            let code = status.as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ConcourseError::Api {
                status: code,
                method: method.to_string(),
                path: path.to_string(),
                message: build_id_hint_for(code, path, &body),
            });
        }
        let body = resp.json::<T>().await?;
        Ok(body)
    }

    pub fn team(&self) -> &str {
        &self.team
    }

    #[tracing::instrument(name = "concourse_client.list_all_pipelines", skip_all)]
    pub async fn list_all_pipelines(&self) -> Result<Vec<Pipeline>, ConcourseError> {
        self.get("/pipelines").await
    }

    #[tracing::instrument(name = "concourse_client.list_pipelines", skip_all)]
    pub async fn list_pipelines(&self) -> Result<Vec<Pipeline>, ConcourseError> {
        let team = &self.team;
        self.get(&format!("/teams/{team}/pipelines")).await
    }

    /// Errors with `ConcourseError::PipelineNotFound` when the pipeline isn't
    /// visible — a previous silent fallback to the configured default team
    /// produced opaque downstream 404s with no signal that the pipeline name
    /// itself was wrong.
    #[tracing::instrument(name = "concourse_client.resolve_pipeline_team", skip_all)]
    pub async fn resolve_pipeline_team(&self, pipeline: &str) -> Result<String, ConcourseError> {
        let pipelines = self.list_all_pipelines().await?;
        if let Some(p) = pipelines.iter().find(|p| p.name == pipeline) {
            return Ok(p.team_name.clone());
        }
        let mut names: Vec<&str> = pipelines.iter().map(|p| p.name.as_str()).collect();
        names.sort_unstable();
        let available = if names.is_empty() {
            "<none visible>".to_string()
        } else {
            const CAP: usize = 30;
            if names.len() > CAP {
                let head = names[..CAP].join(", ");
                let rest = names.len() - CAP;
                format!("{head}, … (+{rest} more)")
            } else {
                names.join(", ")
            }
        };
        Err(ConcourseError::PipelineNotFound {
            pipeline: pipeline.to_string(),
            available,
        })
    }

    #[tracing::instrument(name = "concourse_client.list_jobs", skip_all)]
    pub async fn list_jobs(&self, pipeline: &str) -> Result<Vec<Job>, ConcourseError> {
        let team = self.resolve_pipeline_team(pipeline).await?;
        self.get(&format!("/teams/{team}/pipelines/{pipeline}/jobs"))
            .await
    }

    /// Most recent first.
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

    #[tracing::instrument(name = "concourse_client.get_build", skip_all)]
    pub async fn get_build(&self, build_id: i64) -> Result<Build, ConcourseError> {
        self.get(&format!("/builds/{build_id}")).await
    }

    #[tracing::instrument(name = "concourse_client.get_pipeline_config", skip_all)]
    pub async fn get_pipeline_config(
        &self,
        pipeline: &str,
    ) -> Result<PipelineConfigResponse, ConcourseError> {
        let team = self.resolve_pipeline_team(pipeline).await?;
        self.get(&format!("/teams/{team}/pipelines/{pipeline}/config"))
            .await
    }

    #[tracing::instrument(name = "concourse_client.get_build_resources", skip_all)]
    pub async fn get_build_resources(
        &self,
        build_id: i64,
    ) -> Result<BuildResources, ConcourseError> {
        self.get(&format!("/builds/{build_id}/resources")).await
    }

    /// Streams build events and extracts log lines from `"log"` events. Stops
    /// when an `"end"` event arrives or after a 30s per-chunk read timeout
    /// (SSE streams don't close naturally).
    #[tracing::instrument(name = "concourse_client.get_build_logs", skip_all)]
    pub async fn get_build_logs(&self, build_id: i64) -> Result<String, ConcourseError> {
        use futures_util::StreamExt;
        use std::time::Duration;

        let resp = self.open_build_events(build_id).await?;
        let mut stream = resp.bytes_stream();
        let mut body = String::new();

        loop {
            match tokio::time::timeout(Duration::from_secs(30), stream.next()).await {
                Ok(Some(Ok(chunk))) => {
                    body.push_str(&String::from_utf8_lossy(&chunk));
                    if body.contains("\"event\":\"end\"") {
                        break;
                    }
                }
                Ok(Some(Err(e))) => {
                    tracing::warn!(error = %e, "Error reading build event stream");
                    break;
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }

        let mut logs = String::new();
        extract_logs_from_sse(&body, &mut logs);
        Ok(logs)
    }

    /// Returns the raw response for streaming by [`BuildLogStore`].
    pub(crate) async fn open_build_events(
        &self,
        build_id: i64,
    ) -> Result<reqwest::Response, ConcourseError> {
        self.auth.ensure_token(&self.http, &self.base_url).await?;
        let path = format!("/builds/{build_id}/events");
        let url = self.api_url(&path)?;

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
                method: "GET".to_string(),
                path: path.clone(),
                message: build_id_hint_for(status, &path, &body),
            });
        }

        Ok(resp)
    }

    #[tracing::instrument(name = "concourse_client.trigger_build", skip_all)]
    pub async fn trigger_build(&self, pipeline: &str, job: &str) -> Result<Build, ConcourseError> {
        let team = self.resolve_pipeline_team(pipeline).await?;
        self.post_empty(&format!(
            "/teams/{team}/pipelines/{pipeline}/jobs/{job}/builds"
        ))
        .await
    }
}

/// Concourse returns an empty 404 body for `/builds/{N}/events` and
/// `/teams/.../pipelines/.../jobs/.../builds/{name}/...` lookups, so a
/// bare `HTTP 404` gives the agent no signal. When we recognise a
/// build-scoped path, attach a hint pointing at the global-id-vs-name
/// confusion that's the usual cause.
fn build_id_hint_for(status: u16, path: &str, body: &str) -> String {
    if status == 404 && path.starts_with("/builds/") {
        let extra = "build_id is the global Concourse build id (numeric), \
             not the per-job build name from URLs like `.../jobs/foo/builds/597`. \
             Use list_builds_for_job(pipeline, job) and read `build_id` off the \
             returned build whose `name` matches the URL segment.";
        if body.is_empty() {
            extra.to_string()
        } else {
            format!("{body} | hint: {extra}")
        }
    } else {
        body.to_string()
    }
}

/// Format epoch seconds as `HH:MM:SS` UTC.
pub(crate) fn format_epoch(epoch: i64) -> String {
    let secs = epoch.rem_euclid(86400);
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

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
                if let Some(ts) = envelope.data.get("time").and_then(|v| v.as_i64()) {
                    let stamp = format_epoch(ts);
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
