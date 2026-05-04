use axum::{extract::Request, middleware::Next, response::Response};
use opentelemetry::global;
use opentelemetry_http::HeaderExtractor;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Connects main-server → sandbox spans by extracting W3C traceparent
/// from the inbound request and attaching it to the current tracing
/// span. Mirrors `drua-server`'s `trace_context_middleware`.
pub async fn trace_context_middleware(request: Request, next: Next) -> Response {
    let parent_cx = global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderExtractor(request.headers()))
    });
    let _ = tracing::Span::current().set_parent(parent_cx);
    next.run(request).await
}
