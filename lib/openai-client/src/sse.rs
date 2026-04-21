//! Lightweight SSE (Server-Sent Events) parser for reqwest byte streams.
//!
//! Implements just enough of the SSE protocol to parse OpenAI's streaming
//! responses. Copied from the anthropic-client crate — the SSE framing is
//! provider-agnostic; only the JSON payload differs.

use futures::StreamExt;

/// A parsed SSE event with its type and data payload.
#[derive(Debug, Clone)]
pub struct SseEvent {
    /// Event type (from "event:" field, defaults to "message").
    #[allow(dead_code)]
    pub event: String,
    /// Event data (from "data:" field(s), joined with newlines).
    pub data: String,
}

/// Parse SSE events from a reqwest response's byte stream.
///
/// Collects all SSE events from the stream, calling `handler` for each
/// complete event.
pub async fn parse_sse_stream<S, B, F>(mut stream: S, mut handler: F) -> Result<(), SseError>
where
    S: futures::Stream<Item = Result<B, reqwest::Error>> + Unpin,
    B: AsRef<[u8]>,
    F: FnMut(SseEvent) -> Result<(), SseError>,
{
    let mut buffer = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(SseError::Http)?;
        buffer.push_str(&String::from_utf8_lossy(chunk.as_ref()));

        // Process all complete events in the buffer.
        // An event is terminated by a blank line (\n\n or \r\n\r\n).
        loop {
            let event_end = find_event_boundary(&buffer);
            let Some(end_pos) = event_end else {
                break;
            };

            let event_text = &buffer[..end_pos.text_end];
            if let Some(event) = parse_single_event(event_text) {
                handler(event)?;
            }

            // Remove the processed event + boundary from buffer
            buffer.drain(..end_pos.drain_end);
        }
    }

    // Process any trailing data in the buffer (stream closed mid-event)
    if !buffer.trim().is_empty() {
        if let Some(event) = parse_single_event(&buffer) {
            handler(event)?;
        }
    }

    Ok(())
}

struct EventBoundary {
    /// End of the event text (before the blank-line separator).
    text_end: usize,
    /// Position to drain up to (past the blank-line separator).
    drain_end: usize,
}

/// Find the position of the next event boundary (blank line) in the buffer.
fn find_event_boundary(buf: &str) -> Option<EventBoundary> {
    // Look for \n\n (the standard SSE event separator)
    if let Some(pos) = buf.find("\n\n") {
        return Some(EventBoundary {
            text_end: pos,
            drain_end: pos + 2,
        });
    }
    // Also handle \r\n\r\n
    if let Some(pos) = buf.find("\r\n\r\n") {
        return Some(EventBoundary {
            text_end: pos,
            drain_end: pos + 4,
        });
    }
    None
}

/// Parse a single SSE event from its text (lines between blank-line boundaries).
fn parse_single_event(text: &str) -> Option<SseEvent> {
    let mut event_type = String::from("message");
    let mut data_lines: Vec<&str> = Vec::new();

    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(':') {
            // Comment line — skip
            continue;
        }
        if let Some((field, value)) = line.split_once(':') {
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => event_type = value.to_string(),
                "data" => data_lines.push(value),
                _ => {} // Ignore unknown fields (id, retry, etc.)
            }
        } else if line == "data" {
            // Field with no colon — treat as field with empty value
            data_lines.push("");
        }
    }

    if data_lines.is_empty() {
        return None;
    }

    Some(SseEvent {
        event: event_type,
        data: data_lines.join("\n"),
    })
}

#[derive(Debug, thiserror::Error)]
pub enum SseError {
    #[error("HTTP stream error: {0}")]
    Http(reqwest::Error),
    #[error("Event processing error: {0}")]
    Processing(String),
}
