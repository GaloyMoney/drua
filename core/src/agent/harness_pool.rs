use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use tracing::instrument;

use super::error::AgentError;
use super::sandbox::translate_harness_event;
use super::AgentMessageEvent;
use crate::primitives::AgentId;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

type StdinWriter = Box<dyn tokio::io::AsyncWrite + Unpin + Send>;
type StdoutReader = Box<dyn tokio::io::AsyncRead + Unpin + Send>;

/// A cached connection to a running harness process inside a sandbox pod.
///
/// Holds the stdin/stdout halves of a K8s exec WebSocket session plus the
/// [`sandbox_client::AttachedProcess`] that keeps the WebSocket alive.
struct HarnessSession {
    stdin: StdinWriter,
    stdout: StdoutReader,
    /// Dropping `AttachedProcess` aborts the WebSocket bridge task, so we
    /// must keep it alive for the duration of the session.
    _process: sandbox_client::AttachedProcess,
    last_activity: Instant,
    /// Partial read buffer — may contain an incomplete trailing line between
    /// successive reads from stdout.
    read_buf: Vec<u8>,
}

/// Parameters for sending a message through a pooled harness session.
pub(super) struct HarnessMessage {
    pub agent_id: AgentId,
    pub sandbox_name: String,
    pub client: Arc<sandbox_client::SandboxClient>,
    pub prompt: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub max_turns: Option<u32>,
    pub disallowed_tools: Vec<String>,
}

/// Pool of active harness exec sessions keyed by [`AgentId`].
///
/// Reuses K8s exec WebSocket sessions across messages to eliminate per-message
/// harness startup latency (~1-3 s of Node.js + Claude Agent SDK boot).
///
/// A single background maintenance task handles both idle-session sweeping
/// (sessions unused for [`IDLE_TIMEOUT`]) and heartbeat keepalives (a `\n`
/// every [`HEARTBEAT_INTERVAL`] to prevent K8s 300 s exec idle timeout).
#[derive(Clone)]
pub(super) struct HarnessPool {
    sessions: Arc<Mutex<HashMap<AgentId, Arc<Mutex<HarnessSession>>>>>,
}

impl HarnessPool {
    pub(super) fn new() -> Self {
        let pool = Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        };
        pool.spawn_maintenance();
        pool
    }

    /// Send a message through a pooled (or freshly created) harness session.
    ///
    /// Writes the prompt as a JSON-line to stdin, then reads stdout events
    /// until a terminal event (`result` or `error`) is seen. Events are
    /// forwarded to `tx` as they arrive.
    #[instrument(name = "harness_pool.send_message", skip_all, fields(%msg.agent_id))]
    pub(super) async fn send_message(
        &self,
        msg: HarnessMessage,
        tx: tokio::sync::mpsc::Sender<AgentMessageEvent>,
    ) -> Result<(), AgentError> {
        let agent_id = msg.agent_id;
        let session = self
            .get_or_create(agent_id, &msg.sandbox_name, msg.client)
            .await?;
        let mut guard = session.lock().await;

        let input_line = serde_json::json!({
            "prompt": msg.prompt,
            "session_id": msg.session_id,
            "model": msg.model,
            "max_turns": msg.max_turns,
            "disallowed_tools": msg.disallowed_tools,
        });
        let payload = format!("{}\n", input_line);

        // Write prompt — if the exec session is dead we'll find out here.
        if let Err(e) = guard.stdin.write_all(payload.as_bytes()).await {
            tracing::warn!(error = %e, "Harness stdin write failed, removing session");
            drop(guard);
            self.remove(agent_id).await;
            return Err(AgentError::SandboxExec(format!("stdin write failed: {e}")));
        }
        guard.last_activity = Instant::now();

        // Read stdout events until a terminal event or EOF.
        let mut tmp = [0u8; 4096];
        loop {
            // Drain complete lines already in the buffer.
            while let Some(pos) = guard.read_buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = guard.read_buf.drain(..=pos).collect();
                let line_str = String::from_utf8_lossy(&line).trim().to_string();
                if line_str.is_empty() {
                    continue;
                }
                if let Some(event) = translate_harness_event(&line_str) {
                    let is_terminal = matches!(
                        event,
                        AgentMessageEvent::Done { .. } | AgentMessageEvent::Error { .. }
                    );
                    if tx.send(event).await.is_err() {
                        return Ok(());
                    }
                    if is_terminal {
                        guard.last_activity = Instant::now();
                        return Ok(());
                    }
                }
            }

            // Need more data from stdout.
            match guard.stdout.read(&mut tmp).await {
                Ok(0) => {
                    // EOF — harness process exited.
                    tracing::warn!("Harness stdout EOF, removing session");
                    flush_partial_line(&mut guard.read_buf, &tx).await;
                    drop(guard);
                    self.remove(agent_id).await;
                    return Ok(());
                }
                Ok(n) => {
                    guard.read_buf.extend_from_slice(&tmp[..n]);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Harness stdout read error, removing session");
                    drop(guard);
                    self.remove(agent_id).await;
                    return Err(AgentError::SandboxExec(format!("stdout read failed: {e}")));
                }
            }
        }
    }

    /// Return an existing session or create a fresh exec.
    async fn get_or_create(
        &self,
        agent_id: AgentId,
        sandbox_name: &str,
        client: Arc<sandbox_client::SandboxClient>,
    ) -> Result<Arc<Mutex<HarnessSession>>, AgentError> {
        {
            let sessions = self.sessions.lock().await;
            if let Some(session) = sessions.get(&agent_id) {
                return Ok(Arc::clone(session));
            }
        }

        tracing::info!(%agent_id, %sandbox_name, "Creating new harness session");
        let command = vec!["agent-harness".to_string()];
        let mut process = client.exec_sandbox_raw(sandbox_name, command).await?;

        let stdin = process
            .stdin()
            .ok_or_else(|| AgentError::SandboxExec("no stdin from harness".into()))?;
        let stdout = process
            .stdout()
            .ok_or_else(|| AgentError::SandboxExec("no stdout from harness".into()))?;

        let session = Arc::new(Mutex::new(HarnessSession {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            _process: process,
            last_activity: Instant::now(),
            read_buf: Vec::with_capacity(4096),
        }));

        let mut sessions = self.sessions.lock().await;
        // Another task may have raced us — prefer the existing session.
        if let Some(existing) = sessions.get(&agent_id) {
            return Ok(Arc::clone(existing));
        }
        sessions.insert(agent_id, Arc::clone(&session));
        Ok(session)
    }

    /// Remove a session from the pool, dropping the exec WebSocket.
    async fn remove(&self, agent_id: AgentId) {
        let mut sessions = self.sessions.lock().await;
        if sessions.remove(&agent_id).is_some() {
            tracing::info!(%agent_id, "Harness session removed from pool");
        }
    }

    /// Background task that periodically:
    /// 1. Removes sessions idle for longer than [`IDLE_TIMEOUT`].
    /// 2. Sends a `\n` heartbeat on sessions idle for longer than
    ///    [`HEARTBEAT_INTERVAL`] to prevent K8s exec idle timeout.
    fn spawn_maintenance(&self) {
        let pool = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SWEEP_INTERVAL).await;

                let mut to_remove = Vec::new();
                let mut to_heartbeat = Vec::new();

                {
                    let sessions = pool.sessions.lock().await;
                    for (&agent_id, session) in sessions.iter() {
                        // try_lock: skip sessions currently in use.
                        if let Ok(guard) = session.try_lock() {
                            let idle = guard.last_activity.elapsed();
                            if idle > IDLE_TIMEOUT {
                                to_remove.push(agent_id);
                            } else if idle > HEARTBEAT_INTERVAL {
                                to_heartbeat.push((agent_id, Arc::clone(session)));
                            }
                        }
                    }
                }

                for agent_id in to_remove {
                    tracing::info!(%agent_id, "Sweeping idle harness session");
                    pool.remove(agent_id).await;
                }

                for (agent_id, session) in to_heartbeat {
                    if let Ok(mut guard) = session.try_lock() {
                        if let Err(e) = guard.stdin.write_all(b"\n").await {
                            tracing::warn!(
                                %agent_id,
                                error = %e,
                                "Heartbeat write failed, removing session"
                            );
                            drop(guard);
                            pool.remove(agent_id).await;
                        }
                    }
                }
            }
        });
    }
}

/// Flush any partial trailing line from the read buffer as a final event.
async fn flush_partial_line(buf: &mut Vec<u8>, tx: &tokio::sync::mpsc::Sender<AgentMessageEvent>) {
    if buf.is_empty() {
        return;
    }
    let line_str = String::from_utf8_lossy(buf).trim().to_string();
    buf.clear();
    if !line_str.is_empty() {
        if let Some(event) = translate_harness_event(&line_str) {
            let _ = tx.send(event).await;
        }
    }
}
