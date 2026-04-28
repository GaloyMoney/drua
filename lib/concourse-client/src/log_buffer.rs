use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::RwLock;

use crate::client::{format_epoch, ConcourseClient};
use crate::error::ConcourseError;
use crate::types::BuildEventEnvelope;

/// Idle TTL before a buffer becomes eligible for cleanup.
const STALE_TTL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Serialize)]
pub struct BuildLogResponse {
    pub lines: Vec<String>,
    pub next_offset: usize,
    /// True once the SSE stream has ended.
    pub is_complete: bool,
    /// e.g. "started", "succeeded", "failed".
    pub build_status: String,
}

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

/// In-memory log buffers for live build tailing. The first read for a
/// `build_id` spawns a background task that streams SSE events; subsequent
/// reads return slices using offset/limit pagination.
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

    /// Returns immediately with whatever lines are available; the first call
    /// for a `build_id` spawns the background SSE consumer.
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

    async fn get_or_start(
        &self,
        build_id: i64,
        client: &Arc<ConcourseClient>,
    ) -> Result<Arc<RwLock<BuildLogBuffer>>, ConcourseError> {
        {
            let buffers = self.buffers.read().await;
            if let Some(buf) = buffers.get(&build_id) {
                return Ok(Arc::clone(buf));
            }
        }

        let mut buffers = self.buffers.write().await;

        // Re-check after acquiring the write lock.
        if let Some(buf) = buffers.get(&build_id) {
            return Ok(Arc::clone(buf));
        }

        let buf = Arc::new(RwLock::new(BuildLogBuffer::new()));
        buffers.insert(build_id, Arc::clone(&buf));

        let client = Arc::clone(client);
        let buf_clone = Arc::clone(&buf);
        tokio::spawn(async move {
            stream_build_events(client, build_id, buf_clone).await;
        });

        Ok(buf)
    }

    async fn cleanup_stale(&self) {
        let mut buffers = self.buffers.write().await;
        let now = std::time::Instant::now();
        buffers.retain(|_build_id, buf| {
            // If the buffer is currently locked it's actively in use — keep it.
            match buf.try_read() {
                Ok(guard) => now.duration_since(guard.last_accessed) < STALE_TTL,
                Err(_) => true,
            }
        });
    }
}

async fn stream_build_events(
    client: Arc<ConcourseClient>,
    build_id: i64,
    buffer: Arc<RwLock<BuildLogBuffer>>,
) {
    use futures_util::StreamExt;
    use std::time::Duration;

    let resp = match client.open_build_events(build_id).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(%build_id, error = %e, "Failed to open build events stream");
            let mut buf = buffer.write().await;
            buf.is_complete = true;
            buf.build_status = format!("error: {e}");
            return;
        }
    };

    let mut stream = resp.bytes_stream();
    let mut parser = SseParser::new();

    loop {
        match tokio::time::timeout(Duration::from_secs(30), stream.next()).await {
            Ok(Some(Ok(bytes))) => {
                let text = String::from_utf8_lossy(&bytes);
                let events = parser.feed(&text);
                let mut buf = buffer.write().await;
                for envelope in events {
                    process_envelope(&envelope, &mut buf);
                }
                if buf.is_complete {
                    break;
                }
            }
            Ok(Some(Err(e))) => {
                tracing::warn!(%build_id, error = %e, "Error reading build events stream");
                let mut buf = buffer.write().await;
                buf.is_complete = true;
                break;
            }
            Ok(None) => {
                let mut buf = buffer.write().await;
                let events = parser.flush();
                for envelope in events {
                    process_envelope(&envelope, &mut buf);
                }
                buf.is_complete = true;
                break;
            }
            Err(_) => {
                let mut buf = buffer.write().await;
                let events = parser.flush();
                for envelope in events {
                    process_envelope(&envelope, &mut buf);
                }
                buf.is_complete = true;
                break;
            }
        }
    }
}

fn process_envelope(envelope: &BuildEventEnvelope, buf: &mut BuildLogBuffer) {
    match envelope.event.as_str() {
        "log" => {
            if let Some(payload) = envelope.data.get("payload") {
                if let Some(text) = payload.as_str() {
                    let prefix = envelope
                        .data
                        .get("time")
                        .and_then(|v| v.as_i64())
                        .map(|ts| format!("[{}] ", format_epoch(ts)));
                    for line in text.split('\n') {
                        if !line.is_empty() {
                            match &prefix {
                                Some(p) => buf.lines.push(format!("{p}{line}")),
                                None => buf.lines.push(line.to_string()),
                            }
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
        "finish-task" | "end" => {
            buf.is_complete = true;
        }
        _ => {}
    }
}

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

    fn feed(&mut self, text: &str) -> Vec<BuildEventEnvelope> {
        if self.saw_end_event {
            return Vec::new();
        }

        self.buffer.push_str(text);
        self.extract_events()
    }

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
                // Empty line ends an SSE frame.
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
        }

        events
    }
}
