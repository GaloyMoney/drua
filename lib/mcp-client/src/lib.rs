//! The request boundary shared by direct and tunneled MCP upstream calls.

use std::time::Duration;

use rmcp::{
    model::{CallToolRequest, CallToolRequestParams, CallToolResult, ClientRequest, ServerResult},
    service::{PeerRequestOptions, ServiceError},
    Peer, RoleClient,
};

/// Match the existing server-side tunnel tool-call budget.
pub const UPSTREAM_TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Call a tool with a bounded response wait and protocol-level cancellation.
pub async fn call_tool(
    peer: &Peer<RoleClient>,
    params: CallToolRequestParams,
    timeout: Duration,
) -> Result<CallToolResult, ServiceError> {
    let mut options = PeerRequestOptions::default();
    options.timeout = Some(timeout);
    let response = peer
        .send_request_with_option(
            ClientRequest::CallToolRequest(CallToolRequest::new(params)),
            options,
        )
        .await?
        .await_response()
        .await?;
    match response {
        ServerResult::CallToolResult(result) => Ok(result),
        _ => Err(ServiceError::UnexpectedResponse),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use rmcp::{
        model::{CancelledNotificationParam, Content, ServerCapabilities, ServerInfo},
        service::{NotificationContext, RequestContext},
        transport::{
            streamable_http_client::{
                StreamableHttpClientTransportConfig, StreamableHttpClientWorker,
            },
            streamable_http_server::{
                session::local::LocalSessionManager, StreamableHttpServerConfig,
                StreamableHttpService,
            },
        },
        ErrorData, RoleServer, ServerHandler, ServiceExt,
    };
    use tokio::sync::Notify;

    #[derive(Clone, Default)]
    struct Upstream {
        started: Arc<Notify>,
        cancelled: Arc<Notify>,
    }

    impl ServerHandler for Upstream {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
        }

        async fn call_tool(
            &self,
            params: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<CallToolResult, ErrorData> {
            if params.name == "hang" {
                self.started.notify_one();
                std::future::pending().await
            } else {
                Ok(CallToolResult::success(vec![Content::text("healthy")]))
            }
        }

        async fn on_cancelled(
            &self,
            _params: CancelledNotificationParam,
            _context: NotificationContext<RoleServer>,
        ) {
            self.cancelled.notify_one();
        }
    }

    async fn check_timeout_and_recovery(peer: &Peer<RoleClient>, upstream: &Upstream) {
        let timeout = Duration::from_millis(100);
        let outcome = tokio::time::timeout(
            Duration::from_secs(3),
            call_tool(peer, CallToolRequestParams::new("hang"), timeout),
        )
        .await;
        assert!(matches!(outcome, Ok(Err(ServiceError::Timeout { timeout: actual })) if actual == timeout),
            "an accepted call with no response must return the configured RMCP timeout: {outcome:?}");
        tokio::time::timeout(Duration::from_secs(1), upstream.started.notified())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), upstream.cancelled.notified())
            .await
            .expect("the upstream must receive a cancellation notification");
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            call_tool(peer, CallToolRequestParams::new("healthy"), timeout),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.content[0].as_text().unwrap().text, "healthy");
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_call_times_out_cancels_and_leaves_connection_usable() {
        let upstream = Upstream::default();
        let server = upstream.clone();
        let (server_io, client_io) = tokio::io::duplex(4096);
        let server_task = tokio::spawn(async move {
            server
                .serve(server_io)
                .await
                .unwrap()
                .waiting()
                .await
                .unwrap();
        });
        let client = ().serve(client_io).await.unwrap();
        for _ in 0..3 {
            check_timeout_and_recovery(client.peer(), &upstream).await;
        }
        client.cancel().await.unwrap();
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn streamable_http_call_times_out_and_sends_cancellation() {
        let upstream = Upstream::default();
        let server = upstream.clone();
        let mut config = StreamableHttpServerConfig::default().disable_allowed_hosts();
        config.json_response = true;
        let service = StreamableHttpService::new(
            move || Ok(server.clone()),
            Arc::new(LocalSessionManager::default()),
            config,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = axum::Router::new().nest_service("/mcp", service);
        let server_task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let config = StreamableHttpClientTransportConfig::with_uri(format!("http://{address}/mcp"));
        let worker = StreamableHttpClientWorker::new(reqwest::Client::new(), config);
        let client = ().serve(worker).await.unwrap();
        check_timeout_and_recovery(client.peer(), &upstream).await;
        client.cancel().await.unwrap();
        server_task.abort();
    }
}
