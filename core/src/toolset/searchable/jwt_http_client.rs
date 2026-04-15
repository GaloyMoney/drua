//! rmcp [`StreamableHttpClient`] that mints a fresh short-lived JWT on
//! every outbound request.
//!
//! The upstream reqwest impl takes `auth_token` once at transport
//! construction via `custom_headers`, so without this wrapper every
//! session stays bound to a single JWT and has to be long-lived. Here
//! the `auth_token` passed by the rmcp worker is ignored — we always
//! substitute a newly-minted 60-second token. That keeps tokens short
//! enough to make leakage mostly inert, and there's no "pod uptime >
//! TTL" silent-401 failure mode.

use std::{collections::HashMap, sync::Arc};

use futures::stream::BoxStream;
use http::{HeaderName, HeaderValue};
use rmcp::{
    model::ClientJsonRpcMessage,
    transport::streamable_http_client::{
        StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
    },
};
use sse_stream::{Error as SseError, Sse};

use crate::mcp_jwt::{McpJwtError, McpJwtSigner};

/// Per-request JWT lifetime. Short enough that a leaked token dies
/// before it can be replayed meaningfully; long enough to cover clock
/// skew between galoy-agents and remote Envoys.
const PER_REQUEST_JWT_TTL_SECS: i64 = 60;

#[derive(thiserror::Error, Debug)]
pub enum JwtSigningClientError {
    #[error("reqwest: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("jwt mint: {0}")]
    JwtMint(#[from] McpJwtError),
    #[error("transport: {0}")]
    Transport(String),
}

#[derive(Clone)]
pub struct JwtSigningHttpClient {
    inner: reqwest::Client,
    signer: Arc<McpJwtSigner>,
    audience: Arc<str>,
}

impl JwtSigningHttpClient {
    pub fn new(signer: Arc<McpJwtSigner>, audience: String) -> Self {
        Self {
            inner: reqwest::Client::new(),
            signer,
            audience: Arc::from(audience),
        }
    }

    fn mint(&self) -> Result<String, McpJwtError> {
        self.signer.mint(
            &self.audience,
            self.signer.issuer(),
            PER_REQUEST_JWT_TTL_SECS,
        )
    }
}

/// Lift a `StreamableHttpError<reqwest::Error>` into
/// `StreamableHttpError<JwtSigningClientError>`. The transport variant
/// is non-exhaustive; unrecognized cases collapse into a generic
/// `Client(Transport(...))` rather than fail to compile.
fn lift_err(
    e: StreamableHttpError<reqwest::Error>,
) -> StreamableHttpError<JwtSigningClientError> {
    use StreamableHttpError::*;
    match e {
        Client(e) => Client(JwtSigningClientError::Reqwest(e)),
        Sse(e) => Sse(e),
        Io(e) => Io(e),
        UnexpectedEndOfStream => UnexpectedEndOfStream,
        UnexpectedServerResponse(s) => UnexpectedServerResponse(s),
        UnexpectedContentType(o) => UnexpectedContentType(o),
        ServerDoesNotSupportSse => ServerDoesNotSupportSse,
        ServerDoesNotSupportDeleteSession => ServerDoesNotSupportDeleteSession,
        TokioJoinError(e) => TokioJoinError(e),
        Deserialize(e) => Deserialize(e),
        TransportChannelClosed => TransportChannelClosed,
        MissingSessionIdInResponse => MissingSessionIdInResponse,
        AuthRequired(e) => AuthRequired(e),
        InsufficientScope(e) => InsufficientScope(e),
        ReservedHeaderConflict(s) => ReservedHeaderConflict(s),
        SessionExpired => SessionExpired,
        other => Client(JwtSigningClientError::Transport(other.to_string())),
    }
}

impl StreamableHttpClient for JwtSigningHttpClient {
    type Error = JwtSigningClientError;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let token = self
            .mint()
            .map_err(|e| StreamableHttpError::Client(e.into()))?;
        self.inner
            .post_message(uri, message, session_id, Some(token), custom_headers)
            .await
            .map_err(lift_err)
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let token = self
            .mint()
            .map_err(|e| StreamableHttpError::Client(e.into()))?;
        self.inner
            .delete_session(uri, session_id, Some(token), custom_headers)
            .await
            .map_err(lift_err)
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        _auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        let token = self
            .mint()
            .map_err(|e| StreamableHttpError::Client(e.into()))?;
        self.inner
            .get_stream(uri, session_id, last_event_id, Some(token), custom_headers)
            .await
            .map_err(lift_err)
    }
}
