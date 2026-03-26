use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::RwLock;

use crate::client::ConcourseClient;
use crate::error::ConcourseError;
use crate::types::BuildEventEnvelope;

/// How long a buffer can remain unaccessed before being eligible for cleanup.
const STALE_TTL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Response from an offset-based log read.
#[derive(Debug, Clone, Serialize)]
pub struct BuildLogResponse {
    /// Log lines for the requested range.
    pub lines: Vec<String>,
    /// The offset to use for the next poll.
    pub next_offset: usize,
    /// Whether the SSE stream has ended (build finished).
    pub is_complete: bool,
    /// Current build status string (e.g. "started", "succeeded", "failed").
    pub build_status: String,
}

/// Internal buffer for a single build's log output.
struct BuildLogBuffer {
    lines: Vec<String>,
    is_complete: bool,
    build_status: String,
    last_accessed: std::time::Instant,
}

impl BuildLogBuffer {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            is_complete: false,
            build_status: "pending".to_string(),
            last_accessed: std::time::Instant::now(),
        }
    }
}

/// Manages in-memory log buffers for live build tailing.
///
/// On first request for a build_id, opens an SSE connection via a background
/// tokio task that continuously appends log lines. Subsequent calls return
/// slices from the buffer using offset/limit pagination.
pub struct BuildLogStore {
    buffers: RwLock<HashMap<i64, Arc<RwLock<BuildLogBuffer>>>>,
}

impl Default for BuildLogStore {
    fn default() -> Self {
        Self::new()
    }
}

impl BuildLogStore {
    pub fn new() -> Self {
        Self {
            buffers: RwLock::new(HashMap::new()),
        }
    }

    /// Read log lines for a build with offset-based pagination.
    ///
    /// On the first call for a given `build_id`, spawns a background task to
    /// stream SSE events from Concourse and buffer log lines. Returns
    /// immediately with whatever lines are available.
    #[tracing::instrument(name = "build_log_store.read_logs", skip_all, fields(%build_id))]
    pub async fn read_logs(
        &self,
        build_id: i64,
        client: &Arc<ConcourseClient>,
        offset: usize,
        limit: usize,
    ) -> Result<BuildLogResponse, ConcourseError> {
        self.cleanup_stale().await;

        let buf = self.get_or_start(build_id, client).await?;
        let mut guard = buf.write().await;
        guard.last_accessed = std::time::Instant::now();

        let total = guard.lines.len();
        let start = offset.min(total);
        let end = (start + limit).min(total);
        let lines = guard.lines[start..end].to_vec();
        let next_offset = end;

        Ok(BuildLogResponse {
            lines,
            next_offset,
            is_complete: guard.is_complete,
            build_status: guard.build_status.clone(),
        })
    }

    /// Get an existing buffer or start streaming for this build.
    async fn get_or_start(
        &self,
        build_id: i64,
        client: &Arc<ConcourseClient>,
    ) -> Result<Arc<RwLock<BuildLogBuffer>>, ConcourseError> {
        // Fast path: buffer already exists
        {
            let buffers = self.buffers.read().await;
            if let Some(buf) = buffers.get(&build_id) {
                return Ok(Arc::clone(buf));
            }
        }

        // Slow path: create buffer and start streaming
        let mut buffers = self.buffers.write().await;

        // Double-check after acquiring write lock
        if let Some(buf) = buffers.get(&build_id) {
            return Ok(Arc::clone(buf));
        }

        let buf = Arc::new(RwLock::new(BuildLogBuffer::new()));
        buffers.insert(build_id, Arc::clone(&buf));

        // Spawn background SSE consumer
        let client = Arc::clone(client);
        let buf_clone = Arc::clone(&buf);
        tokio::spawn(async move {
            stream_build_events(client, build_id, buf_clone).await;
        });

        Ok(buf)
    }

    /// Remove buffers that haven't been accessed within the TTL.
    async fn cleanup_stale(&self) {
        let mut buffers = self.buffers.write().await;
        let now = std::time::Instant::now();
        buffers.retain(|_build_id, buf| {
            // Try to read-lock without blocking; if locked, keep the entry
            match buf.try_read() {
                Ok(guard) => now.duration_since(guard.last_accessed) < STALE_TTL,
                Err(_) => true, // Buffer is actively in use
            }
        });
    }
}

/// Background task: opens SSE stream and appends log lines to the buffer.
async fn stream_build_events(
    client: Arc<ConcourseClient>,
    build_id: i64,
    buffer: Arc<RwLock<BuildLogBuffer>>,
) {
    let mut resp = match client.open_build_events(build_id).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(%build_id, error = %e, "Failed to open build events stream");
            let mut buf = buffer.write().await;
            buf.is_complete = true;
            buf.build_status = format!("error: {e}");
            return;
        }
    };

    let mut parser = SseParser::new();

    loop {
        match resp.chunk().await {
            Ok(Some(bytes)) => {
                let text = String::from_utf8_lossy(&bytes);
                let events = parser.feed(&text);
                let mut buf = buffer.write().await;
                for envelope in events {
                    process_envelope(&envelope, &mut buf);
                }
            }
            Ok(None) => {
                // Stream ended
                let mut buf = buffer.write().await;
                // Process any remaining buffered data
                let events = parser.flush();
                for envelope in events {
                    process_envelope(&envelope, &mut buf);
                }
                buf.is_complete = true;
                break;
            }
            Err(e) => {
                tracing::warn!(%build_id, error = %e, "Error reading build events stream");
                let mut buf = buffer.write().await;
                buf.is_complete = true;
                break;
            }
        }
    }
}

/// Process a single SSE event envelope into the buffer.
fn process_envelope(envelope: &BuildEventEnvelope, buf: &mut BuildLogBuffer) {
    match envelope.event.as_str() {
        "log" => {
            if let Some(payload) = envelope.data.get("payload") {
                if let Some(text) = payload.as_str() {
                    // Split multi-line payloads into individual lines
                    for line in text.split('\n') {
                        if !line.is_empty() {
                            buf.lines.push(line.to_string());
                        }
                    }
                }
            }
        }
        "status" => {
            if let Some(status) = envelope.data.get("status") {
                if let Some(s) = status.as_str() {
                    buf.build_status = s.to_string();
                }
            }
        }
        _ => {}
    }
}

/// Incremental SSE parser that processes chunks of text into complete events.
struct SseParser {
    buffer: String,
    current_data: String,
    saw_end_event: bool,
}

impl SseParser {
    fn new() -> Self {
        Self {
            buffer: String::new(),
            current_data: String::new(),
            saw_end_event: false,
        }
    }

    /// Feed a chunk of text and return any complete events.
    fn feed(&mut self, text: &str) -> Vec<BuildEventEnvelope> {
        if self.saw_end_event {
            return Vec::new();
        }

        self.buffer.push_str(text);
        self.extract_events()
    }

    /// Flush any remaining buffered data as events.
    fn flush(&mut self) -> Vec<BuildEventEnvelope> {
        if self.current_data.is_empty() {
            return Vec::new();
        }
        let data = std::mem::take(&mut self.current_data);
        match serde_json::from_str::<BuildEventEnvelope>(&data) {
            Ok(env) => vec![env],
            Err(_) => Vec::new(),
        }
    }

    fn extract_events(&mut self) -> Vec<BuildEventEnvelope> {
        let mut events = Vec::new();

        while let Some(newline_pos) = self.buffer.find('\n') {
            let line = self.buffer[..newline_pos]
                .trim_end_matches('\r')
                .to_string();
            self.buffer.drain(..=newline_pos);

            if line.is_empty() {
                // Empty line = end of SSE frame, dispatch event
                if !self.current_data.is_empty() {
                    let data = std::mem::take(&mut self.current_data);
                    if let Ok(env) = serde_json::from_str::<BuildEventEnvelope>(&data) {
                        events.push(env);
                    }
                }
                continue;
            }

            if let Some(value) = line.strip_prefix("event:") {
                let event_type = value.trim();
                if event_type == "end" {
                    self.saw_end_event = true;
                    return events;
                }
            } else if let Some(value) = line.strip_prefix("data:") {
                if !self.current_data.is_empty() {
                    self.current_data.push('\n');
                }
                self.current_data.push_str(value.trim());
            }
            // Ignore id:, comment lines, and unknown fields
        }

        events
    }
}
