//! Minimal SSE parser for reqwest byte streams. Just enough of the
//! protocol to handle Anthropic's streaming responses.

use futures::StreamExt;

#[derive(Debug, Clone)]
pub struct SseEvent {
    /// "event:" field, defaults to "message".
    pub event: String,
    /// "data:" lines joined with `\n`.
    pub data: String,
}

/// Calls `handler` for each complete event in the stream.
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

        // Events are terminated by a blank line (\n\n or \r\n\r\n).
        loop {
            let event_end = find_event_boundary(&buffer);
            let Some(end_pos) = event_end else {
                break;
            };

            let event_text = &buffer[..end_pos.text_end];
            if let Some(event) = parse_single_event(event_text) {
                handler(event)?;
            }

            buffer.drain(..end_pos.drain_end);
        }
    }

    // Trailing data: stream closed mid-event.
    if !buffer.trim().is_empty() {
        if let Some(event) = parse_single_event(&buffer) {
            handler(event)?;
        }
    }

    Ok(())
}

struct EventBoundary {
    text_end: usize,
    drain_end: usize,
}

fn find_event_boundary(buf: &str) -> Option<EventBoundary> {
    if let Some(pos) = buf.find("\n\n") {
        return Some(EventBoundary {
            text_end: pos,
            drain_end: pos + 2,
        });
    }
    if let Some(pos) = buf.find("\r\n\r\n") {
        return Some(EventBoundary {
            text_end: pos,
            drain_end: pos + 4,
        });
    }
    None
}

fn parse_single_event(text: &str) -> Option<SseEvent> {
    let mut event_type = String::from("message");
    let mut data_lines: Vec<&str> = Vec::new();

    for line in text.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some((field, value)) = line.split_once(':') {
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => event_type = value.to_string(),
                "data" => data_lines.push(value),
                _ => {}
            }
        } else if line == "data" {
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
