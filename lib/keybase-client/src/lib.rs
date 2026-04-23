use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt;
use tracing::instrument;

// ── Incoming types (keybase chat api-listen) ────────────────────────────────

/// A single event from `keybase chat api-listen` (one JSON object per line).
#[derive(Deserialize, Debug)]
pub struct ChatEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub msg: Option<ChatMessage>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ChatMessage {
    pub id: u64,
    pub conversation_id: String,
    pub channel: Channel,
    pub sender: Sender,
    pub sent_at: Option<u64>,
    pub content: Content,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Channel {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub members_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic_type: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Sender {
    pub uid: Option<String>,
    pub username: String,
    pub device_id: Option<String>,
    pub device_name: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Content {
    #[serde(rename = "type")]
    pub type_: String,
    pub text: Option<TextContent>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct TextContent {
    pub body: String,
}

// ── Outgoing types (keybase chat api -m) ────────────────────────────────────

#[derive(Serialize)]
struct SendCommand {
    method: String,
    params: SendParams,
}

#[derive(Serialize)]
struct SendParams {
    options: SendOptions,
}

#[derive(Serialize)]
struct SendOptions {
    channel: Channel,
    message: MessageBody,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<u64>,
}

#[derive(Serialize)]
struct MessageBody {
    body: String,
}

// ── Response types ──────────────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
pub struct SendResponse {
    pub result: Option<SendResult>,
    pub error: Option<ApiError>,
}

#[derive(Deserialize, Debug)]
pub struct SendResult {
    pub id: Option<u64>,
}

#[derive(Deserialize, Debug)]
pub struct ApiError {
    pub code: i32,
    pub message: String,
}

// ── Error ───────────────────────────────────────────────────────────────────

#[derive(thiserror::Error, Debug)]
pub enum KeybaseClientError {
    #[error("KeybaseClientError - IO: {0}")]
    Io(#[from] std::io::Error),
    #[error("KeybaseClientError - JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("KeybaseClientError - Api: {message}")]
    Api { code: i32, message: String },
    #[error("KeybaseClientError - Utf8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
}

// ── Client ──────────────────────────────────────────────────────────────────

/// Wraps the `keybase` CLI binary for chat operations.
#[derive(Clone)]
pub struct KeybaseClient {
    keybase_path: String,
}

impl KeybaseClient {
    pub fn new(keybase_path: impl Into<String>) -> Self {
        Self {
            keybase_path: keybase_path.into(),
        }
    }

    /// Authenticate the bot with `keybase oneshot`.
    ///
    /// The paperkey is passed via the `KEYBASE_PAPERKEY` environment variable
    /// (not as a CLI argument) to avoid leaking it in `ps` output.
    #[instrument(name = "keybase_client.login", skip_all)]
    pub async fn login(&self, username: &str, paperkey: &str) -> Result<(), KeybaseClientError> {
        let output = tokio::process::Command::new(&self.keybase_path)
            .args(["oneshot", "--username", username])
            .env("KEYBASE_PAPERKEY", paperkey)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(KeybaseClientError::Api {
                code: output.status.code().unwrap_or(-1),
                message: format!("keybase oneshot failed: {stderr}"),
            });
        }

        tracing::info!("Keybase bot logged in as {username}");
        Ok(())
    }

    /// Send a message to a channel.
    #[instrument(name = "keybase_client.send_message", skip_all)]
    pub async fn send_message(
        &self,
        channel: &Channel,
        body: &str,
    ) -> Result<SendResponse, KeybaseClientError> {
        self.send(channel, body, None).await
    }

    /// Send a threaded reply.
    #[instrument(name = "keybase_client.send_reply", skip_all)]
    pub async fn send_reply(
        &self,
        channel: &Channel,
        body: &str,
        reply_to: u64,
    ) -> Result<SendResponse, KeybaseClientError> {
        self.send(channel, body, Some(reply_to)).await
    }

    /// Start listening for chat events.
    ///
    /// Spawns `keybase chat api-listen` and returns a receiver of parsed events.
    /// The background reader task runs until the child process exits or the
    /// receiver is dropped.
    #[instrument(name = "keybase_client.listen", skip_all)]
    pub fn listen(
        &self,
    ) -> Result<
        tokio::sync::mpsc::UnboundedReceiver<Result<ChatEvent, KeybaseClientError>>,
        KeybaseClientError,
    > {
        let mut child = tokio::process::Command::new(&self.keybase_path)
            .args(["chat", "api-listen"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .stdin(std::process::Stdio::null())
            .spawn()?;

        let stdout = child
            .stdout
            .take()
            .expect("stdout was piped but is missing");
        let reader = tokio::io::BufReader::new(stdout);

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        tokio::spawn(async move {
            let mut lines = reader.lines();
            loop {
                let line = match lines.next_line().await {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(e) => {
                        let _ = tx.send(Err(KeybaseClientError::Io(e)));
                        break;
                    }
                };

                if line.trim().is_empty() {
                    continue;
                }

                let event =
                    serde_json::from_str::<ChatEvent>(&line).map_err(KeybaseClientError::Json);
                if tx.send(event).is_err() {
                    break;
                }
            }

            // Ensure the child process is cleaned up
            let _ = child.kill().await;
        });

        Ok(rx)
    }

    // ── Internal ────────────────────────────────────────────────────────

    async fn send(
        &self,
        channel: &Channel,
        body: &str,
        reply_to: Option<u64>,
    ) -> Result<SendResponse, KeybaseClientError> {
        let cmd = SendCommand {
            method: "send".into(),
            params: SendParams {
                options: SendOptions {
                    channel: channel.clone(),
                    message: MessageBody {
                        body: body.to_owned(),
                    },
                    reply_to,
                },
            },
        };
        let json = serde_json::to_string(&cmd)?;

        let output = tokio::process::Command::new(&self.keybase_path)
            .args(["chat", "api", "-m", &json])
            .output()
            .await?;

        let stdout = String::from_utf8(output.stdout)?;
        let resp: SendResponse = serde_json::from_str(&stdout)?;

        if let Some(err) = &resp.error {
            return Err(KeybaseClientError::Api {
                code: err.code,
                message: err.message.clone(),
            });
        }

        Ok(resp)
    }
}

impl Default for KeybaseClient {
    fn default() -> Self {
        Self::new("keybase")
    }
}
