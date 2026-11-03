use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde::Serialize;
use url::Url;

use crate::error::ZendutyError;
use crate::types::*;

const MAX_RETRIES: u32 = 2;
const MAX_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(5);
const BASE_BACKOFF: std::time::Duration = std::time::Duration::from_millis(500);

fn retry_delay(attempt: u32, retry_after: Option<&str>) -> std::time::Duration {
    retry_after
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(std::time::Duration::from_secs)
        .unwrap_or_else(|| BASE_BACKOFF * 2u32.pow(attempt.min(4)))
        .min(MAX_RETRY_AFTER)
}

/// Async client for the Zenduty REST API.
pub struct ZendutyClient {
    http: reqwest::Client,
    base_url: Url,
}

impl ZendutyClient {
    /// `base_url` defaults to `https://www.zenduty.com/` if empty.
    /// `api_token` is sent as `Authorization: Token <token>` per Zenduty's docs.
    pub fn new(base_url: &str, api_token: &str) -> Result<Self, ZendutyError> {
        let base = if base_url.trim().is_empty() {
            "https://www.zenduty.com/"
        } else {
            base_url
        };
        let mut base_url = Url::parse(base)?;
        // Ensure trailing slash so `Url::join` keeps the prefix.
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }

        let mut headers = HeaderMap::new();
        let auth = format!("Token {}", api_token.trim());
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&auth).map_err(|e| ZendutyError::InvalidHeader(e.to_string()))?,
        );
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let http = reqwest::Client::builder()
            .user_agent("zenduty-client-rs/0.1")
            .default_headers(headers)
            .build()?;

        Ok(Self { http, base_url })
    }

    fn api_url(&self, path: &str) -> Result<Url, ZendutyError> {
        let trimmed = path.trim_start_matches('/');
        Ok(self.base_url.join(trimmed)?)
    }

    /// Sends a freshly built request per attempt (a `RequestBuilder` can't be
    /// reused after `send`). Retries up to `MAX_RETRIES` times on 429, and on
    /// 502/503 when `idempotent` (a gateway error on a write may have been
    /// applied). Honours `Retry-After` seconds, capped at `MAX_RETRY_AFTER`,
    /// else backs off 500 ms, 1 s. The final response is returned as-is so
    /// exhaustion still surfaces as the usual error.
    async fn send_with_retry(
        &self,
        idempotent: bool,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, ZendutyError> {
        let mut attempt: u32 = 0;
        loop {
            let resp = build().send().await?;
            let status = resp.status().as_u16();
            let retryable = status == 429 || (idempotent && matches!(status, 502 | 503));
            if !retryable || attempt >= MAX_RETRIES {
                return Ok(resp);
            }
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok());
            let delay = retry_delay(attempt, retry_after);
            tracing::warn!(
                attempt = attempt + 1,
                status,
                delay_ms = delay.as_millis() as u64,
                "zenduty-client: retrying after transient HTTP error"
            );
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, ZendutyError> {
        let url = self.api_url(path)?;
        let resp = self
            .send_with_retry(true, || self.http.get(url.clone()))
            .await?;
        Self::handle_response(resp).await
    }

    async fn post_json<T, B>(&self, path: &str, body: &B) -> Result<T, ZendutyError>
    where
        T: serde::de::DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let url = self.api_url(path)?;
        let resp = self
            .send_with_retry(false, || self.http.post(url.clone()).json(body))
            .await?;
        Self::handle_response(resp).await
    }

    async fn post_json_with_query<T, B, Q>(
        &self,
        path: &str,
        body: &B,
        query: &Q,
    ) -> Result<T, ZendutyError>
    where
        T: serde::de::DeserializeOwned,
        B: Serialize + ?Sized,
        Q: Serialize + ?Sized,
    {
        let url = self.api_url(path)?;
        let resp = self
            .send_with_retry(false, || {
                self.http.post(url.clone()).query(query).json(body)
            })
            .await?;
        Self::handle_response(resp).await
    }

    async fn patch_json<T, B>(&self, path: &str, body: &B) -> Result<T, ZendutyError>
    where
        T: serde::de::DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let url = self.api_url(path)?;
        let resp = self
            .send_with_retry(false, || self.http.patch(url.clone()).json(body))
            .await?;
        Self::handle_response(resp).await
    }

    async fn handle_response<T: serde::de::DeserializeOwned>(
        resp: reqwest::Response,
    ) -> Result<T, ZendutyError> {
        let status = resp.status();
        if status.is_success() {
            let content_type = response_content_type(&resp);
            let body = resp.text().await?;
            if !is_json_content_type(&content_type) {
                return Err(ZendutyError::UnexpectedResponse {
                    status: status.as_u16(),
                    content_type,
                    body_preview: body_preview(&body),
                });
            }
            return serde_json::from_str::<T>(&body).map_err(ZendutyError::Json);
        }
        Err(Self::error_for_status(resp).await)
    }

    /// DELETE returns 204 No Content — no body to deserialize.
    async fn handle_no_content(resp: reqwest::Response) -> Result<(), ZendutyError> {
        if resp.status().is_success() {
            return Ok(());
        }
        Err(Self::error_for_status(resp).await)
    }

    async fn error_for_status(resp: reqwest::Response) -> ZendutyError {
        let code = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        let body = body_preview(&body);
        match code {
            401 | 403 => ZendutyError::Unauthorized(body),
            404 => ZendutyError::NotFound(body),
            429 => ZendutyError::RateLimited(body),
            _ => ZendutyError::Api {
                status: code,
                message: body,
            },
        }
    }

    #[tracing::instrument(name = "zenduty_client.list_incidents", skip_all)]
    pub async fn list_incidents(
        &self,
        params: &ListIncidentsParams<'_>,
    ) -> Result<Vec<Incident>, ZendutyError> {
        let mut q: Vec<(&str, String)> = Vec::new();
        if let Some(page) = params.page {
            q.push(("page", page.to_string()));
        }
        if let Some(page_size) = params.page_size {
            q.push(("page_size", page_size.to_string()));
        }
        let body = IncidentFilter {
            status: incident_filter_status(params.status),
            team_ids: params.team_id.into_iter().collect(),
            all_teams: 1,
            service_ids: params.service_id.into_iter().collect(),
            user_ids: Vec::new(),
            priority_name: "",
            priority_ids: Vec::new(),
            tag_ids: Vec::new(),
            sla_ids: Vec::new(),
            from_date: Vec::new(),
            to_date: Vec::new(),
            postmortem_filter: -1,
            escalation_policy_ids: Vec::new(),
        };
        match self
            .post_json_with_query::<Page<Incident>, _, _>("/api/incidents/filter/", &body, &q)
            .await
        {
            Ok(page) => Ok(page.into_results()),
            // Zenduty returns 404 + {"detail":"Invalid page."} when `page`
            // is past the last page; standard list semantics is an empty page.
            Err(e) if e.is_invalid_page_overshoot() => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    #[tracing::instrument(name = "zenduty_client.get_incident", skip_all)]
    pub async fn get_incident(&self, incident_id: &str) -> Result<Incident, ZendutyError> {
        self.get(&format!("/api/incidents/{incident_id}/")).await
    }

    #[tracing::instrument(name = "zenduty_client.add_incident_note", skip_all)]
    pub async fn add_incident_note(
        &self,
        incident_id: &str,
        note: &str,
    ) -> Result<IncidentNote, ZendutyError> {
        let body = NewIncidentNote { note };
        self.post_json(&format!("/api/incidents/{incident_id}/note/"), &body)
            .await
    }

    #[tracing::instrument(name = "zenduty_client.list_incident_notes", skip_all)]
    pub async fn list_incident_notes(
        &self,
        incident_id: &str,
    ) -> Result<Vec<IncidentNote>, ZendutyError> {
        let page: Page<IncidentNote> = self
            .get(&format!("/api/incidents/{incident_id}/note/"))
            .await?;
        Ok(page.into_results())
    }

    #[tracing::instrument(name = "zenduty_client.delete_incident_note", skip_all)]
    pub async fn delete_incident_note(
        &self,
        incident_id: &str,
        note_id: &str,
    ) -> Result<(), ZendutyError> {
        let url = self.api_url(&format!("/api/incidents/{incident_id}/note/{note_id}/"))?;
        let resp = self
            .send_with_retry(true, || self.http.delete(url.clone()))
            .await?;
        Self::handle_no_content(resp).await
    }

    #[tracing::instrument(name = "zenduty_client.update_incident_status", skip_all)]
    pub async fn update_incident_status(
        &self,
        incident_id: &str,
        status: IncidentStatus,
    ) -> Result<Incident, ZendutyError> {
        let body = IncidentStatusUpdate {
            status: Some(status.as_u8()),
        };
        self.patch_json(&format!("/api/incidents/{incident_id}/"), &body)
            .await
    }

    pub async fn acknowledge_incident(&self, incident_id: &str) -> Result<Incident, ZendutyError> {
        self.update_incident_status(incident_id, IncidentStatus::Acknowledged)
            .await
    }

    pub async fn resolve_incident(&self, incident_id: &str) -> Result<Incident, ZendutyError> {
        self.update_incident_status(incident_id, IncidentStatus::Resolved)
            .await
    }

    #[tracing::instrument(name = "zenduty_client.list_schedules", skip_all)]
    pub async fn list_schedules(&self, team_id: &str) -> Result<Vec<Schedule>, ZendutyError> {
        let page: Page<Schedule> = self
            .get(&format!("/api/account/teams/{team_id}/schedules/"))
            .await?;
        Ok(page.into_results())
    }

    #[tracing::instrument(name = "zenduty_client.get_schedule", skip_all)]
    pub async fn get_schedule(
        &self,
        team_id: &str,
        schedule_id: &str,
    ) -> Result<Schedule, ZendutyError> {
        let mut schedule: Schedule = self
            .get(&format!(
                "/api/account/teams/{team_id}/schedules/{schedule_id}/"
            ))
            .await?;
        schedule.on_call_now = self
            .get(&format!("/api/account/teams/{team_id}/oncall/"))
            .await?;
        Ok(schedule)
    }
}

fn response_content_type(resp: &reqwest::Response) -> String {
    resp.headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("<missing>")
        .to_string()
}

fn is_json_content_type(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .map(str::trim)
        .is_some_and(|mime| mime.eq_ignore_ascii_case("application/json"))
}

fn body_preview(body: &str) -> String {
    const MAX_CHARS: usize = 500;
    let mut preview: String = body.chars().take(MAX_CHARS).collect();
    if body.chars().count() > MAX_CHARS {
        preview.push_str("...");
    }
    preview
}

fn incident_filter_status(statuses: Option<&[u8]>) -> i8 {
    let mut statuses = statuses.unwrap_or(&[]).to_vec();
    statuses.sort_unstable();
    statuses.dedup();

    match statuses.as_slice() {
        [] | [1, 2] => -1,
        [1, 2, 3] => 0,
        [status @ 1..=3] => *status as i8,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;
    use axum::Router;

    use super::{incident_filter_status, retry_delay, ZendutyClient};
    use crate::error::ZendutyError;

    /// Serves `GET /api/incidents/abc/` with `respond(attempt)` ->
    /// `(status, retry_after)`; returns the client and the attempt counter.
    async fn fake_zenduty(
        respond: impl Fn(u32) -> (StatusCode, Option<&'static str>) + Send + Sync + 'static,
    ) -> (ZendutyClient, Arc<AtomicU32>) {
        let attempts = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&attempts);
        let respond = Arc::new(respond);
        let app = Router::new().route(
            "/api/incidents/abc/",
            get(move || {
                let attempt = counter.fetch_add(1, Ordering::SeqCst) + 1;
                let (status, retry_after) = respond(attempt);
                async move {
                    let mut headers = HeaderMap::new();
                    headers.insert("content-type", "application/json".parse().unwrap());
                    if let Some(ra) = retry_after {
                        headers.insert("retry-after", ra.parse().unwrap());
                    }
                    let body = if status.is_success() {
                        r#"{"title":"disk full"}"#
                    } else {
                        r#"{"detail":"Too many requests"}"#
                    };
                    (status, headers, body)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = ZendutyClient::new(&format!("http://{addr}/"), "tok").unwrap();
        (client, attempts)
    }

    #[tokio::test]
    async fn get_incident_retries_once_on_429_then_succeeds() {
        let (client, attempts) = fake_zenduty(|attempt| {
            if attempt == 1 {
                (StatusCode::TOO_MANY_REQUESTS, Some("0"))
            } else {
                (StatusCode::OK, None)
            }
        })
        .await;
        let incident = client
            .get_incident("abc")
            .await
            .expect("retried to success");
        assert_eq!(incident.title, "disk full");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn get_incident_exhausts_retries_and_surfaces_rate_limited() {
        let (client, attempts) = fake_zenduty(|_| (StatusCode::TOO_MANY_REQUESTS, Some("0"))).await;
        let err = client
            .get_incident("abc")
            .await
            .expect_err("never succeeds");
        assert!(matches!(err, ZendutyError::RateLimited(_)), "{err}");
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            3,
            "1 try + 2 retries, no more"
        );
    }

    #[tokio::test]
    async fn non_retryable_status_is_not_retried() {
        let (client, attempts) = fake_zenduty(|_| (StatusCode::NOT_FOUND, None)).await;
        assert!(client.get_incident("abc").await.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn retry_delay_honours_retry_after_with_a_cap() {
        assert_eq!(retry_delay(0, Some("2")), Duration::from_secs(2));
        assert_eq!(retry_delay(0, Some("3600")), Duration::from_secs(5));
        assert_eq!(retry_delay(0, Some("0")), Duration::ZERO);
    }

    #[test]
    fn retry_delay_backs_off_without_a_usable_header() {
        assert_eq!(retry_delay(0, None), Duration::from_millis(500));
        assert_eq!(retry_delay(1, None), Duration::from_secs(1));
        assert_eq!(
            retry_delay(1, Some("Wed, 21 Oct 2026 07:28:00 GMT")),
            Duration::from_secs(1)
        );
        assert_eq!(retry_delay(30, None), Duration::from_secs(5));
    }

    #[test]
    fn incident_filter_status_maps_zenduty_filter_codes() {
        assert_eq!(incident_filter_status(None), -1);
        assert_eq!(incident_filter_status(Some(&[1, 2])), -1);
        assert_eq!(incident_filter_status(Some(&[1, 2, 3])), 0);
        assert_eq!(incident_filter_status(Some(&[1])), 1);
        assert_eq!(incident_filter_status(Some(&[2])), 2);
        assert_eq!(incident_filter_status(Some(&[3])), 3);
    }
}
