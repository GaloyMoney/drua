use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use tokio::sync::RwLock;
use url::Url;

use crate::error::ConcourseError;
use crate::types::TokenResponse;

/// `RwLock`-based interior mutability so the client can use `&self`.
pub(crate) struct AuthManager {
    username: String,
    password: String,
    token: RwLock<Option<String>>,
}

impl AuthManager {
    pub fn new(username: String, password: String) -> Self {
        Self {
            username,
            password,
            token: RwLock::new(None),
        }
    }

    pub async fn ensure_token(
        &self,
        http: &reqwest::Client,
        base_url: &Url,
    ) -> Result<(), ConcourseError> {
        {
            let token = self.token.read().await;
            if token.is_some() {
                return Ok(());
            }
        }

        let mut token = self.token.write().await;
        if token.is_some() {
            return Ok(());
        }

        let token_url = base_url.join("/sky/issuer/token")?;

        // Concourse uses Dex with client_id="fly", client_secret="Zmx5".
        let resp = http
            .post(token_url)
            .basic_auth("fly", Some("Zmx5"))
            .form(&[
                ("grant_type", "password"),
                ("username", self.username.as_str()),
                ("password", self.password.as_str()),
                ("scope", "openid profile email federated:id groups"),
            ])
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(ConcourseError::Auth(format!(
                "token request failed (HTTP {status}): {body}"
            )));
        }

        let token_resp: TokenResponse = resp.json().await?;
        *token = Some(token_resp.access_token);
        Ok(())
    }

    pub async fn clear_token(&self) {
        let mut token = self.token.write().await;
        *token = None;
    }

    pub async fn auth_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let token = self.token.read().await;
        if let Some(token) = token.as_ref() {
            if let Ok(val) = HeaderValue::from_str(&format!("Bearer {token}")) {
                headers.insert(AUTHORIZATION, val);
            }
        }
        headers
    }
}
