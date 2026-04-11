use std::time::Duration;

use opentelemetry::global;
use opentelemetry_http::HeaderInjector;
use tracing::instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::session::NewSessionEvent;

use super::error::AgentError;
use super::sandbox::{translate_harness_event, translate_to_session_events};
use super::AgentMessageEvent;

const HARNESS_PORT: u16 = 3000;
/// Maximum time a single agentic run can take (many tool calls).
/// Set to 30 minutes — with SSE keepalive heartbeats preventing idle-stream
/// closure, this acts as a safety-net upper bound.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(1800);

/// HTTP client for the agent harness running inside sandbox pods.
///
/// Replaces the previous `HarnessPool` (K8s exec WebSocket sessions) with
/// simple HTTP POST + SSE streaming via the pod's serviceFQDN.
#[derive(Clone)]
pub(super) struct HarnessClient {
    http: reqwest::Client,
}

impl HarnessClient {
    pub(super) fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("failed to build reqwest client"),
        }
    }

    /// Wait for the harness HTTP server to become healthy.
    #[instrument(name = "harness_client.wait_healthy", skip_all, fields(%service_fqdn))]
    pub(super) async fn wait_healthy(
        &self,
        service_fqdn: &str,
        timeout: Duration,
    ) -> Result<(), AgentError> {
        let url = format!("http://{service_fqdn}:{HARNESS_PORT}/health");
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            match self
                .http
                .get(&url)
                .timeout(Duration::from_secs(5))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => return Ok(()),
                _ => {}
            }

            if tokio::time::Instant::now() >= deadline {
                return Err(AgentError::SandboxExec(
                    "Harness health check timed out".to_string(),
                ));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Send a message to the harness and stream SSE events back through `tx`.
    ///
    /// If `session_tx` is provided, raw harness events are translated into
    /// rich [`NewSessionEvent`]s and forwarded for persistence.
    #[instrument(name = "harness_client.send_message", skip_all, fields(%service_fqdn))]
    pub(super) async fn send_message(
        &self,
        service_fqdn: &str,
        input: serde_json::Value,
        tx: tokio::sync::mpsc::Sender<AgentMessageEvent>,
        session_tx: Option<tokio::sync::mpsc::Sender<NewSessionEvent>>,
    ) -> Result<(), AgentError> {
        let url = format!("http://{service_fqdn}:{HARNESS_PORT}/message");

        // Inject W3C traceparent from the current tracing span so the
        // harness can create child spans that join the distributed trace.
        let cx = tracing::Span::current().context();
        let mut headers = reqwest::header::HeaderMap::new();
        global::get_text_map_propagator(|propagator| {
            propagator.inject_context(&cx, &mut HeaderInjector(&mut headers));
        });

        let mut response = self
            .http
            .post(&url)
            .headers(headers)
            .json(&input)
            .send()
            .await
            .map_err(|e| AgentError::SandboxExec(format!("HTTP request to harness failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(AgentError::SandboxExec(format!(
                "Harness returned {status}: {body}"
            )));
        }

        // Read SSE stream chunk by chunk.  Each SSE event is:
        //   event: <type>\n
        //   data: <json>\n
        //   \n
        let mut buf = String::new();

        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| AgentError::SandboxExec(format!("SSE stream read error: {e}")))?
        {
            buf.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete SSE events (delimited by double newline).
            while let Some(pos) = buf.find("\n\n") {
                let event_block = &buf[..pos];

                if let Some(data_line) = event_block.lines().find(|l| l.starts_with("data: ")) {
                    let json_str = &data_line["data: ".len()..];

                    // Translate raw line into session events (captures ALL types)
                    if let Some(ref stx) = session_tx {
                        let raw = serde_json::from_str::<serde_json::Value>(json_str).ok();
                        let session_events = translate_to_session_events(json_str, raw);
                        for se in session_events {
                            let _ = stx.send(se).await;
                        }
                    }

                    // Translate into SSE agent events (text/tool_call/result/done/error)
                    let events = translate_harness_event(json_str);
                    for event in events {
                        let is_terminal = matches!(
                            event,
                            AgentMessageEvent::Done { .. } | AgentMessageEvent::Error { .. }
                        );
                        if tx.send(event).await.is_err() {
                            return Ok(());
                        }
                        if is_terminal {
                            return Ok(());
                        }
                    }
                }

                // Advance past the consumed event + the "\n\n" delimiter.
                let drain_to = pos + 2;
                buf = buf[drain_to..].to_string();
            }
        }

        Ok(())
    }
}
