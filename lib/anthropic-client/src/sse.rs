//! Lightweight SSE (Server-Sent Events) parser for reqwest byte streams.
//!
//! Implements just enough of the SSE protocol to parse Anthropic's streaming
//! responses. Adapted from the Pi agent crate's full SSE implementation but
//! simplified to work with `reqwest::Response::bytes_stream()`.

use futures::StreamExt;

/// A parsed SSE event with its type and data payload.
#[derive(Debug)]
pub(crate) struct SseEvent {
    /// Event type (from "event:" field, defaults to "message").
    pub event: String,
    /// Event data (from "data:" field(s), joined with newlines).
    pub data: String,
}

/// Parse SSE events from a reqwest response's byte stream.
///
/// Collects all SSE events from the stream, calling `handler` for each
/// complete event. This is a pull-based approach that processes the entire
/// stream — appropriate because `send_prompt` needs the final accumulated
/// result anyway.
pub(crate) async fn parse_sse_stream<S, B, F>(mut stream: S, mut handler: F) -> Result<(), SseError>
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
pub(crate) enum SseError {
    #[error("HTTP stream error: {0}")]
    Http(reqwest::Error),
    #[error("Event processing error: {0}")]
    Processing(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_event() {
        let text = "event: message_start\ndata: {\"type\":\"message_start\"}";
        let event = parse_single_event(text).unwrap();
        assert_eq!(event.event, "message_start");
        assert_eq!(event.data, "{\"type\":\"message_start\"}");
    }

    #[test]
    fn parse_event_with_default_type() {
        let text = "data: hello";
        let event = parse_single_event(text).unwrap();
        assert_eq!(event.event, "message");
        assert_eq!(event.data, "hello");
    }

    #[test]
    fn parse_event_with_multiple_data_lines() {
        let text = "data: line1\ndata: line2\ndata: line3";
        let event = parse_single_event(text).unwrap();
        assert_eq!(event.data, "line1\nline2\nline3");
    }

    #[test]
    fn skip_comment_lines() {
        let text = ": this is a comment\ndata: payload";
        let event = parse_single_event(text).unwrap();
        assert_eq!(event.data, "payload");
    }

    #[test]
    fn no_event_without_data() {
        let text = "event: ping";
        assert!(parse_single_event(text).is_none());
    }

    #[test]
    fn find_boundary_lf() {
        let buf = "data: hello\n\ndata: world\n\n";
        let b = find_event_boundary(buf).unwrap();
        assert_eq!(&buf[..b.text_end], "data: hello");
        assert_eq!(b.drain_end, 13);
    }

    #[test]
    fn find_boundary_crlf() {
        let buf = "data: hello\r\n\r\ndata: world\r\n\r\n";
        let b = find_event_boundary(buf).unwrap();
        assert_eq!(&buf[..b.text_end], "data: hello");
        assert_eq!(b.drain_end, 15);
    }
}
