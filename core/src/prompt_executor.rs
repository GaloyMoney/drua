use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::instrument;

use anthropic_client::AnthropicClient;
use llm::provider::LlmProvider;
use llm::router::{walk, ChainEntry};
use llm::{
    Prompt, PromptError, PromptRequest, PromptRequestChannel, PromptResponseChannel, PromptResult,
    StreamHandle,
};
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
    /// Logs diagnostics. Never fails — executor must boot with empty keys
    /// (local dev, bats tests); the first real prompt will 401 upstream.
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

/// First 7 + last 4 chars, rest masked. Spots copy/paste errors without leaking secrets.
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

/// Drop aborts the task; lets the executor live as long as its owning service.
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

/// Drains `PromptRequest`s off a channel and streams responses back via each
/// request's `response_channel`. Drop aborts the worker task.
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

    fn find(&self, name: &str) -> Option<&ResolvedModel> {
        self.models.iter().find(|m| m.name == name)
    }

    /// Unknown model ids are skipped with a warn; only an entirely
    /// unknown chain errors.
    fn build_chain(&self, prompt: &Prompt) -> Result<Vec<ChainEntry>, PromptError> {
        let mut entries = Vec::with_capacity(prompt.chain.len());
        for spec in prompt.chain.iter() {
            match self.find(&spec.name) {
                Some(model) => entries.push(ChainEntry {
                    spec: llm::ModelSpec {
                        name: spec.name.clone(),
                        max_tokens: spec.max_tokens.or(model.default_max_tokens),
                    },
                    provider: model.client.clone(),
                }),
                None => {
                    tracing::warn!(
                        model = %spec.name,
                        "Chain element references model not in registry; skipping"
                    );
                }
            }
        }
        if entries.is_empty() {
            return Err(PromptError::ModelNotConfigured(
                prompt.chain.primary.name.clone(),
            ));
        }
        Ok(entries)
    }

    #[instrument(name = "domain.prompt_executor.dispatch", skip_all)]
    fn dispatch(&self, request: PromptRequest) {
        let entries = match self.build_chain(&request.prompt) {
            Ok(e) => e,
            Err(e) => {
                tracing::error!(error = %e, "Chain resolution failed");
                let _ = request.response_channel.send(Err(e));
                return;
            }
        };

        tracing::info!(
            primary = %request.prompt.chain.primary.name,
            chain_len = entries.len(),
            "Dispatching prompt with chain",
        );

        // StreamHandle handoff is deferred until the first upstream Ok
        // so transient failures can fall through to the next entry.
        let response_channel = request.response_channel;
        let prompt = request.prompt;
        tokio::spawn(async move {
            evaluate_chain(entries, prompt, response_channel).await;
        });
    }
}

#[instrument(name = "domain.prompt_executor.evaluate_chain", skip_all)]
async fn evaluate_chain(entries: Vec<ChainEntry>, prompt: Prompt, response: PromptResponseChannel) {
    match walk(&entries, &prompt).await {
        Ok(outcome) => {
            tracing::info!(
                model_used = %outcome.model_used,
                attempts = outcome.attempts.len(),
                "router.walk: success",
            );
            // TODO: thread `outcome.model_used` through PromptResult so
            // the agent loop can record the winning fallback id.
            let _ = response.send(Ok(PromptResult::Stream(StreamHandle {
                rx: outcome.stream.rx,
            })));
        }
        Err(e) => {
            tracing::error!(error = %e, "router.walk: chain exhausted or terminal");
            let _ = response.send(Err(e));
        }
    }
}

#[derive(Clone)]
struct ResolvedModel {
    name: String,
    default_max_tokens: Option<u32>,
    client: Arc<dyn LlmProvider>,
}

impl ResolvedModel {
    fn from_config(config: ModelConfig) -> Self {
        let client: Arc<dyn LlmProvider> = match config.provider {
            Provider::Anthropic { api_key, base_url } => {
                Arc::new(AnthropicClient::new(api_key).with_base_url(base_url))
            }
            Provider::OpenAi { api_key, base_url } => {
                Arc::new(OpenAiClient::new(api_key).with_base_url(base_url))
            }
            Provider::OpenAiResponses { auth, base_url } => Arc::new(
                OpenAiResponsesClient::new(match auth {
                    OpenAiResponsesAuth::ApiKey { api_key } => {
                        ClientOpenAiResponsesAuth::ApiKey { api_key }
                    }
                    OpenAiResponsesAuth::Subscription => ClientOpenAiResponsesAuth::Subscription,
                })
                .with_base_url(base_url),
            ),
        };
        Self {
            name: config.name,
            default_max_tokens: config.default_max_tokens,
            client,
        }
    }
}
