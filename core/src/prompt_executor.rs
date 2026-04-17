use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::instrument;

use anthropic_client::AnthropicClient;
use llm::{Prompt, PromptError, PromptRequest, PromptRequestChannel, PromptResponseChannel};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptExecutorConfig {
    #[serde(default)]
    pub models: Vec<ModelConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub name: String,
    pub provider: Provider,
    #[serde(default)]
    pub default_max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Provider {
    Anthropic { api_key: String },
}

#[derive(Debug, thiserror::Error)]
pub enum PromptExecutorConfigError {}

impl PromptExecutorConfig {
    /// Log diagnostics about each configured model's credential. Never
    /// fails — the executor must boot even with an empty key (e.g. local
    /// dev, bats tests) so the rest of the surface (auth, MCP gateway,
    /// search) is reachable; the first real prompt will just 401 at the
    /// upstream.
    pub fn validate(&self) -> Result<(), PromptExecutorConfigError> {
        for model in &self.models {
            match &model.provider {
                Provider::Anthropic { api_key } if api_key.is_empty() => {
                    tracing::warn!(
                        model = %model.name,
                        "Anthropic credential is empty — agent prompts will fail until ANTHROPIC_API_KEY is set",
                    );
                }
                Provider::Anthropic { api_key } => {
                    // Anthropic production keys start with `sk-ant-`. A
                    // different prefix is usually a paste-swap with some
                    // other vendor's key (OpenAI, AWS, etc.) and will
                    // always 401 at the Messages API. Warn loudly and log
                    // a masked preview so the operator can eyeball the
                    // value that actually reached the process.
                    let preview = masked_preview(api_key);
                    if !api_key.starts_with("sk-ant-") {
                        tracing::warn!(
                            model = %model.name,
                            key_preview = %preview,
                            key_len = api_key.len(),
                            "Anthropic credential does not start with `sk-ant-` — this will most likely 401 at the Messages API",
                        );
                    } else {
                        tracing::info!(
                            model = %model.name,
                            key_preview = %preview,
                            "Anthropic credential loaded"
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

/// First 7 + last 4 chars, everything else masked. Enough to spot
/// copy/paste errors without spilling secrets into logs.
fn masked_preview(s: &str) -> String {
    if s.len() <= 12 {
        "***".to_string()
    } else {
        let head: String = s.chars().take(7).collect();
        let tail: String = s
            .chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        format!("{head}…{tail}")
    }
}

/// Wraps a `tokio::task::JoinHandle` so that dropping the wrapper aborts the
/// task. Lets the executor live as long as the owning service struct.
struct OwnedTaskHandle(Option<JoinHandle<()>>);

impl OwnedTaskHandle {
    fn new(inner: JoinHandle<()>) -> Self {
        Self(Some(inner))
    }
}

impl Drop for OwnedTaskHandle {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

/// Long-running service that drains `PromptRequest`s off a channel and
/// streams `PromptResponseEvent`s back via each request's `response_channel`.
///
/// `init` spawns the worker task immediately and returns the service plus the
/// sender half that producers (e.g. the `Agents` service) should be handed.
/// When the `PromptExecutor` is dropped the worker task is aborted.
pub struct PromptExecutor {
    _handle: OwnedTaskHandle,
}

impl PromptExecutor {
    #[instrument(name = "domain.prompt_executor.init", skip_all)]
    pub async fn init(config: PromptExecutorConfig) -> (Self, PromptRequestChannel) {
        let state = Arc::new(ExecutorState::from_config(config));
        let (tx, rx) = mpsc::channel(64);
        let handle = tokio::spawn(Self::run(state, rx));
        (
            Self {
                _handle: OwnedTaskHandle::new(handle),
            },
            tx,
        )
    }

    async fn run(state: Arc<ExecutorState>, mut requests: mpsc::Receiver<PromptRequest>) {
        while let Some(request) = requests.recv().await {
            state.dispatch(request);
        }
    }
}

struct ExecutorState {
    models: Vec<ResolvedModel>,
}

impl ExecutorState {
    fn from_config(config: PromptExecutorConfig) -> Self {
        let models = config
            .models
            .into_iter()
            .map(ResolvedModel::from_config)
            .collect();
        Self { models }
    }

    #[instrument(name = "domain.prompt_executor.dispatch", skip_all)]
    fn dispatch(&self, mut request: PromptRequest) {
        let model_name = request.prompt.model.clone();
        let model = self.models.iter().find(|m| m.name == model_name).cloned();
        match model {
            None => {
                let _ = request
                    .response_channel
                    .send(Err(PromptError::Provider(format!(
                        "model `{model_name}` not configured"
                    ))));
            }
            Some(model) => {
                if request.prompt.max_tokens.is_none() {
                    request.prompt.max_tokens = model.default_max_tokens;
                }
                match model.client {
                    ProviderClient::Anthropic(client) => {
                        tokio::spawn(async move {
                            evaluate_with_anthropic_streaming(
                                client,
                                request.prompt,
                                request.response_channel,
                            )
                            .await;
                        });
                    }
                }
            }
        }
    }
}

#[instrument(name = "domain.prompt_executor.evaluate_streaming", skip_all)]
async fn evaluate_with_anthropic_streaming(
    client: AnthropicClient,
    prompt: Prompt,
    response: PromptResponseChannel,
) {
    let (delta_tx, delta_rx) = mpsc::channel(128);

    // Send StreamHandle immediately so agent loop can start consuming.
    if response
        .send(Ok(llm::PromptResult::Stream(llm::StreamHandle {
            rx: delta_rx,
        })))
        .is_err()
    {
        return; // caller dropped
    }

    // Start streaming from Anthropic.
    match client.send_prompt_streaming(&prompt).await {
        Ok(mut sse_rx) => {
            while let Some(sse_result) = sse_rx.recv().await {
                match sse_result {
                    Ok(sse_event) => match map_sse_to_delta(&sse_event.data) {
                        Ok(Some(delta)) => {
                            if delta_tx.send(Ok(delta)).await.is_err() {
                                break;
                            }
                        }
                        Ok(None) => {} // ping or unknown event
                        Err(e) => {
                            let _ = delta_tx.send(Err(PromptError::Provider(e))).await;
                            break;
                        }
                    },
                    Err(e) => {
                        let _ = delta_tx
                            .send(Err(PromptError::Provider(e.to_string())))
                            .await;
                        break;
                    }
                }
            }
        }
        Err(e) => {
            let _ = delta_tx
                .send(Err(PromptError::Provider(e.to_string())))
                .await;
        }
    }
}

fn map_sse_to_delta(data: &str) -> Result<Option<llm::stream::StreamDelta>, String> {
    use anthropic_client::types::*;
    use llm::stream::{ContentBlockType, StreamDelta};

    let event: AnthropicStreamEvent =
        serde_json::from_str(data).map_err(|e| format!("JSON parse: {e}"))?;
    Ok(match event {
        AnthropicStreamEvent::MessageStart { message } => {
            let input_tokens = message.usage.map(|u| u.input as u32).unwrap_or(0);
            Some(StreamDelta::MessageStart { input_tokens })
        }
        AnthropicStreamEvent::ContentBlockStart {
            index,
            content_block,
        } => {
            let block_type = match content_block {
                AnthropicContentBlock::Text => ContentBlockType::Text,
                AnthropicContentBlock::ToolUse { id, name } => ContentBlockType::ToolUse {
                    id: id.unwrap_or_default(),
                    name: name.unwrap_or_default(),
                },
                AnthropicContentBlock::Thinking => ContentBlockType::Thinking,
            };
            Some(StreamDelta::ContentBlockStart {
                index: index as usize,
                block_type,
            })
        }
        AnthropicStreamEvent::ContentBlockDelta { index, delta } => {
            let idx = index as usize;
            match delta {
                AnthropicDelta::TextDelta { text } => text.map(|t| StreamDelta::TextDelta {
                    index: idx,
                    text: t,
                }),
                AnthropicDelta::ThinkingDelta { thinking } => {
                    thinking.map(|t| StreamDelta::ThinkingDelta {
                        index: idx,
                        text: t,
                    })
                }
                AnthropicDelta::InputJsonDelta { partial_json } => {
                    partial_json.map(|j| StreamDelta::InputJsonDelta {
                        index: idx,
                        partial_json: j,
                    })
                }
                AnthropicDelta::SignatureDelta { signature } => {
                    signature.map(|s| StreamDelta::SignatureDelta {
                        index: idx,
                        signature: s,
                    })
                }
            }
        }
        AnthropicStreamEvent::ContentBlockStop { index } => Some(StreamDelta::ContentBlockStop {
            index: index as usize,
        }),
        AnthropicStreamEvent::MessageDelta { delta, usage } => {
            let stop_reason = delta.stop_reason.map(|sr| match sr {
                AnthropicStopReason::EndTurn => llm::StopReason::EndTurn,
                AnthropicStopReason::MaxTokens => llm::StopReason::MaxTokens,
                AnthropicStopReason::ToolUse => llm::StopReason::ToolUse,
                AnthropicStopReason::StopSequence => llm::StopReason::StopSequence,
            });
            let output_tokens = usage.map(|u| u.output_tokens as u32).unwrap_or(0);
            Some(StreamDelta::MessageDelta {
                stop_reason,
                output_tokens,
            })
        }
        AnthropicStreamEvent::MessageStop => Some(StreamDelta::MessageStop),
        AnthropicStreamEvent::Error { error } => Some(StreamDelta::Error {
            message: error.message,
        }),
        AnthropicStreamEvent::Ping => None,
    })
}

#[derive(Clone)]
struct ResolvedModel {
    name: String,
    default_max_tokens: Option<u32>,
    client: ProviderClient,
}

impl ResolvedModel {
    fn from_config(config: ModelConfig) -> Self {
        let client = match config.provider {
            Provider::Anthropic { api_key } => {
                ProviderClient::Anthropic(AnthropicClient::new(api_key))
            }
        };
        Self {
            name: config.name,
            default_max_tokens: config.default_max_tokens,
            client,
        }
    }
}

#[derive(Clone)]
enum ProviderClient {
    Anthropic(AnthropicClient),
}
