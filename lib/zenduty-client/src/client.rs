use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde::Serialize;
use url::Url;

use crate::error::ZendutyError;
use crate::types::*;

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

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, ZendutyError> {
        let url = self.api_url(path)?;
        let resp = self.http.get(url).send().await?;
        Self::handle_response(resp).await
    }

    async fn get_with_query<T, Q>(&self, path: &str, query: &Q) -> Result<T, ZendutyError>
    where
        T: serde::de::DeserializeOwned,
        Q: Serialize + ?Sized,
    {
        let url = self.api_url(path)?;
        let resp = self.http.get(url).query(query).send().await?;
        Self::handle_response(resp).await
    }

    async fn post_json<T, B>(&self, path: &str, body: &B) -> Result<T, ZendutyError>
    where
        T: serde::de::DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let url = self.api_url(path)?;
        let resp = self.http.post(url).json(body).send().await?;
        Self::handle_response(resp).await
    }

    async fn patch_json<T, B>(&self, path: &str, body: &B) -> Result<T, ZendutyError>
    where
        T: serde::de::DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let url = self.api_url(path)?;
        let resp = self.http.patch(url).json(body).send().await?;
        Self::handle_response(resp).await
    }

    async fn handle_response<T: serde::de::DeserializeOwned>(
        resp: reqwest::Response,
    ) -> Result<T, ZendutyError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp.json::<T>().await?);
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
        if let Some(statuses) = params.status {
            for s in statuses {
                q.push(("status", s.to_string()));
            }
        }
        if let Some(team) = params.team_id {
            q.push(("team", team.to_string()));
        }
        if let Some(service) = params.service_id {
            q.push(("service", service.to_string()));
        }
        if let Some(page) = params.page {
            q.push(("page", page.to_string()));
        }
        if let Some(page_size) = params.page_size {
            q.push(("page_size", page_size.to_string()));
        }
        let page: Page<Incident> = self.get_with_query("/api/incidents/", &q).await?;
        Ok(page.into_results())
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
        let resp = self.http.delete(url).send().await?;
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
        self.get(&format!(
            "/api/account/teams/{team_id}/schedules/{schedule_id}/"
        ))
        .await
    }
}
