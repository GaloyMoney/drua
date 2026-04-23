use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::instrument;

use anthropic_client::AnthropicClient;
use llm::prompt::CacheTtl;
use llm::provider::LlmProvider;
use llm::{Prompt, PromptError, PromptRequest, PromptRequestChannel, PromptResponseChannel};
use openai_client::{
    OpenAiClient, OpenAiResponsesAuth as ClientOpenAiResponsesAuth, OpenAiResponsesClient,
};

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
    Anthropic {
        api_key: String,
        #[serde(default)]
        base_url: Option<String>,
    },
    OpenAi {
        api_key: String,
        #[serde(default)]
        base_url: Option<String>,
    },
    OpenAiResponses {
        auth: OpenAiResponsesAuth,
        #[serde(default)]
        base_url: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OpenAiResponsesAuth {
    ApiKey { api_key: String },
    Subscription,
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
                Provider::Anthropic { api_key, .. } if api_key.is_empty() => {
                    tracing::warn!(
                        model = %model.name,
                        "Anthropic credential is empty — agent prompts will fail until ANTHROPIC_API_KEY is set",
                    );
                }
                Provider::Anthropic { api_key, base_url } => {
                    let preview = masked_preview(api_key);
                    if base_url.is_some() {
                        tracing::info!(
                            model = %model.name,
                            key_preview = %preview,
                            base_url = %base_url.as_deref().unwrap_or("default"),
                            "Anthropic credential loaded (custom endpoint — key prefix validation skipped)"
                        );
                    } else if !api_key.starts_with("sk-ant-") {
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
                Provider::OpenAi { api_key, .. } if api_key.is_empty() => {
                    tracing::warn!(
                        model = %model.name,
                        "OpenAI credential is empty — agent prompts will fail until OPENAI_API_KEY is set",
                    );
                }
                Provider::OpenAi { api_key, base_url } => {
                    let preview = masked_preview(api_key);
                    if base_url.is_some() {
                        tracing::info!(
                            model = %model.name,
                            key_preview = %preview,
                            base_url = %base_url.as_deref().unwrap_or("default"),
                            "OpenAI credential loaded (custom endpoint — key prefix validation skipped)"
                        );
                    } else if !api_key.starts_with("sk-") {
                        tracing::warn!(
                            model = %model.name,
                            key_preview = %preview,
                            key_len = api_key.len(),
                            "OpenAI credential does not start with `sk-` — this will most likely 401 at the Chat Completions API",
                        );
                    } else {
                        tracing::info!(
                            model = %model.name,
                            key_preview = %preview,
                            "OpenAI credential loaded"
                        );
                    }
                }
                Provider::OpenAiResponses { auth, base_url } => match auth {
                    OpenAiResponsesAuth::ApiKey { api_key } if api_key.is_empty() => {
                        tracing::warn!(
                            model = %model.name,
                            "OpenAI Responses credential is empty — agent prompts will fail until OPENAI_API_KEY is set",
                        );
                    }
                    OpenAiResponsesAuth::ApiKey { api_key } => {
                        let preview = masked_preview(api_key);
                        if base_url.is_some() {
                            tracing::info!(
                                model = %model.name,
                                key_preview = %preview,
                                base_url = %base_url.as_deref().unwrap_or("default"),
                                "OpenAI Responses credential loaded (custom endpoint — key prefix validation skipped)"
                            );
                        } else if !api_key.starts_with("sk-") {
                            tracing::warn!(
                                model = %model.name,
                                key_preview = %preview,
                                key_len = api_key.len(),
                                "OpenAI Responses credential does not start with `sk-` — this will most likely 401 at the Responses API",
                            );
                        } else {
                            tracing::info!(
                                model = %model.name,
                                key_preview = %preview,
                                "OpenAI Responses credential loaded"
                            );
                        }
                    }
                    OpenAiResponsesAuth::Subscription => {
                        tracing::info!(
                            model = %model.name,
                            "OpenAI subscription credential will be resolved from OPENAI_CODEX_ACCESS_TOKEN or ~/.codex/auth.json at request time",
                        );
                    }
                },
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
                tracing::error!(model = %model_name, "Model not configured");
                let _ = request
                    .response_channel
                    .send(Err(PromptError::Provider(format!(
                        "model `{model_name}` not configured"
                    ))));
            }
            Some(model) => {
                tracing::info!(
                    model = %model.name,
                    provider = model.client.name(),
                    "Dispatching prompt",
                );
                if request.prompt.max_tokens.is_none() {
                    request.prompt.max_tokens = model.default_max_tokens;
                }
                request.prompt = model.prepare_prompt(request.prompt);
                let client = model.client.clone();
                tokio::spawn(async move {
                    evaluate_streaming(client, request.prompt, request.response_channel).await;
                });
            }
        }
    }
}

#[instrument(name = "domain.prompt_executor.evaluate_streaming", skip_all)]
async fn evaluate_streaming(
    client: Arc<dyn LlmProvider>,
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

    match client.send_prompt_streaming(&prompt).await {
        Ok(mut provider_rx) => {
            tracing::debug!(provider = client.name(), "Stream started");
            while let Some(result) = provider_rx.recv().await {
                if delta_tx.send(result).await.is_err() {
                    break;
                }
            }
            tracing::debug!(provider = client.name(), "Stream completed");
        }
        Err(e) => {
            tracing::error!(provider = client.name(), error = %e, "Provider error");
            let _ = delta_tx.send(Err(e)).await;
        }
    }
}

#[derive(Clone)]
struct ResolvedModel {
    name: String,
    default_max_tokens: Option<u32>,
    provider_kind: ResolvedProviderKind,
    client: Arc<dyn LlmProvider>,
}

#[derive(Clone, Copy)]
enum ResolvedProviderKind {
    Anthropic,
    OpenAi,
    OpenAiResponses,
}

impl ResolvedModel {
    fn from_config(config: ModelConfig) -> Self {
        let (provider_kind, client): (ResolvedProviderKind, Arc<dyn LlmProvider>) =
            match config.provider {
                Provider::Anthropic { api_key, base_url } => (
                    ResolvedProviderKind::Anthropic,
                    Arc::new(AnthropicClient::new(api_key).with_base_url(base_url)),
                ),
                Provider::OpenAi { api_key, base_url } => (
                    ResolvedProviderKind::OpenAi,
                    Arc::new(OpenAiClient::new(api_key).with_base_url(base_url)),
                ),
                Provider::OpenAiResponses { auth, base_url } => (
                    ResolvedProviderKind::OpenAiResponses,
                    Arc::new(
                        OpenAiResponsesClient::new(match auth {
                            OpenAiResponsesAuth::ApiKey { api_key } => {
                                ClientOpenAiResponsesAuth::ApiKey { api_key }
                            }
                            OpenAiResponsesAuth::Subscription => {
                                ClientOpenAiResponsesAuth::Subscription
                            }
                        })
                        .with_base_url(base_url),
                    ),
                ),
            };
        Self {
            name: config.name,
            default_max_tokens: config.default_max_tokens,
            provider_kind,
            client,
        }
    }

    fn prepare_prompt(&self, mut prompt: Prompt) -> Prompt {
        if matches!(self.provider_kind, ResolvedProviderKind::Anthropic) {
            prompt.enable_anthropic_prompt_caching(Some(CacheTtl::FiveMinutes));
        }
        prompt
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm::prompt::{AssistantBlock, CacheControl, Message, UserBlock};

    fn sample_prompt() -> Prompt {
        Prompt {
            model: "test-model".to_string(),
            system: Vec::new(),
            messages: vec![Message::Assistant {
                content: vec![
                    AssistantBlock::Thinking {
                        text: "thinking".to_string(),
                        signature: None,
                    },
                    AssistantBlock::Text {
                        text: "visible".to_string(),
                        cache_control: None,
                    },
                ],
            }],
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: None,
            cache_key: Some("agent-session:test".to_string()),
        }
    }

    #[test]
    fn prepare_prompt_marks_anthropic_cache_breakpoint() {
        let model = ResolvedModel {
            name: "test-model".to_string(),
            default_max_tokens: None,
            provider_kind: ResolvedProviderKind::Anthropic,
            client: Arc::new(AnthropicClient::new("test")),
        };

        let prompt = model.prepare_prompt(sample_prompt());
        match &prompt.messages[0] {
            Message::Assistant { content } => {
                assert!(matches!(
                    &content[1],
                    AssistantBlock::Text {
                        cache_control: Some(CacheControl::Ephemeral {
                            ttl: Some(CacheTtl::FiveMinutes)
                        }),
                        ..
                    }
                ));
            }
            _ => panic!("expected assistant message"),
        }
    }

    #[test]
    fn prepare_prompt_leaves_openai_prompt_unmarked() {
        let model = ResolvedModel {
            name: "test-model".to_string(),
            default_max_tokens: None,
            provider_kind: ResolvedProviderKind::OpenAi,
            client: Arc::new(OpenAiClient::new("test")),
        };

        let prompt = model.prepare_prompt(Prompt {
            messages: vec![Message::User {
                content: vec![UserBlock::Text {
                    text: "hello".to_string(),
                    cache_control: None,
                }],
            }],
            ..sample_prompt()
        });

        match &prompt.messages[0] {
            Message::User { content } => {
                assert!(matches!(
                    &content[0],
                    UserBlock::Text {
                        cache_control: None,
                        ..
                    }
                ));
            }
            _ => panic!("expected user message"),
        }
    }
}
