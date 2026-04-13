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
pub enum PromptExecutorConfigError {
    #[error("model `{model}` is configured with provider `{provider}` but its credential is empty (e.g. set ANTHROPIC_API_KEY)")]
    EmptyCredential { model: String, provider: String },
}

impl PromptExecutorConfig {
    /// Catch obvious misconfig at startup time so we don't wait until the
    /// first agent message to get a 401 back from the upstream provider.
    pub fn validate(&self) -> Result<(), PromptExecutorConfigError> {
        for model in &self.models {
            match &model.provider {
                Provider::Anthropic { api_key } if api_key.is_empty() => {
                    return Err(PromptExecutorConfigError::EmptyCredential {
                        model: model.name.clone(),
                        provider: "anthropic".to_string(),
                    });
                }
                Provider::Anthropic { .. } => {}
            }
        }
        Ok(())
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
                            evaluate_with_anthropic(
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

async fn evaluate_with_anthropic(
    client: AnthropicClient,
    prompt: Prompt,
    response: PromptResponseChannel,
) {
    let outcome = client
        .send_prompt(&prompt)
        .await
        .map_err(|e| PromptError::Provider(e.to_string()));
    let _ = response.send(outcome);
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
