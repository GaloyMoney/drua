//! Provider-agnostic chain walker. Transient errors advance; terminal
//! errors abort. The walker never mutates the prompt — a single chain
//! may freely mix providers.

use std::sync::Arc;

use crate::provider::LlmProvider;
use crate::request::PromptError;
use crate::{Prompt, StreamHandle};

#[derive(Clone)]
pub struct ChainEntry {
    pub model_id: String,
    /// Today only `max_tokens` is plumbed through per-attempt.
    pub max_tokens: Option<u32>,
    pub provider: Arc<dyn LlmProvider>,
}

impl std::fmt::Debug for ChainEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainEntry")
            .field("model_id", &self.model_id)
            .field("max_tokens", &self.max_tokens)
            .field("provider", &self.provider.name())
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct AttemptRecord {
    pub model_id: String,
    pub provider_name: String,
    pub outcome: AttemptOutcome,
}

#[derive(Debug, Clone)]
pub enum AttemptOutcome {
    Succeeded,
    Transient(String),
    Terminal(String),
}

#[derive(Debug)]
pub struct WalkOutcome {
    pub stream: StreamHandle,
    pub model_used: String,
    pub attempts: Vec<AttemptRecord>,
}

/// First `Ok` from `send_prompt_streaming` wins. Mid-stream errors are
/// not recoverable — falling back after deltas leak would corrupt the
/// assistant message in the session log.
pub async fn walk(chain: &[ChainEntry], base: &Prompt) -> Result<WalkOutcome, PromptError> {
    if chain.is_empty() {
        return Err(PromptError::Provider("empty chain".to_string()));
    }

    let mut attempts: Vec<AttemptRecord> = Vec::with_capacity(chain.len());
    let mut last_err: Option<PromptError> = None;

    for entry in chain {
        let mut prompt = base.clone();
        if let Some(mt) = entry.max_tokens {
            prompt.max_tokens = Some(mt);
        }

        match entry.provider.send_prompt_streaming(&prompt).await {
            Ok(rx) => {
                attempts.push(AttemptRecord {
                    model_id: entry.model_id.clone(),
                    provider_name: entry.provider.name().to_string(),
                    outcome: AttemptOutcome::Succeeded,
                });
                tracing::info!(
                    model = %entry.model_id,
                    provider = entry.provider.name(),
                    attempts = attempts.len(),
                    "router.walk: stream opened",
                );
                return Ok(WalkOutcome {
                    stream: StreamHandle { rx },
                    model_used: entry.model_id.clone(),
                    attempts,
                });
            }
            Err(e) if e.is_terminal() => {
                attempts.push(AttemptRecord {
                    model_id: entry.model_id.clone(),
                    provider_name: entry.provider.name().to_string(),
                    outcome: AttemptOutcome::Terminal(e.to_string()),
                });
                tracing::error!(
                    model = %entry.model_id,
                    provider = entry.provider.name(),
                    error = %e,
                    "router.walk: terminal error, aborting chain",
                );
                return Err(e);
            }
            Err(e) => {
                tracing::warn!(
                    model = %entry.model_id,
                    provider = entry.provider.name(),
                    error = %e,
                    "router.walk: transient error, trying next",
                );
                attempts.push(AttemptRecord {
                    model_id: entry.model_id.clone(),
                    provider_name: entry.provider.name().to_string(),
                    outcome: AttemptOutcome::Transient(e.to_string()),
                });
                last_err = Some(e);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| PromptError::Provider("chain exhausted".to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::LlmProvider;
    use crate::request::{TerminalKind, TransientKind};
    use crate::stream::StreamDelta;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    struct OkProvider {
        name: &'static str,
    }
    #[async_trait]
    impl LlmProvider for OkProvider {
        fn name(&self) -> &str {
            self.name
        }
        async fn send_prompt_streaming(
            &self,
            _prompt: &Prompt,
        ) -> Result<mpsc::Receiver<Result<StreamDelta, PromptError>>, PromptError> {
            let (_tx, rx) = mpsc::channel(1);
            Ok(rx)
        }
    }

    struct FailingProvider {
        name: &'static str,
        err: PromptError,
        calls: AtomicUsize,
    }
    impl FailingProvider {
        fn new(name: &'static str, err: PromptError) -> Self {
            Self {
                name,
                err,
                calls: AtomicUsize::new(0),
            }
        }
    }
    #[async_trait]
    impl LlmProvider for FailingProvider {
        fn name(&self) -> &str {
            self.name
        }
        async fn send_prompt_streaming(
            &self,
            _prompt: &Prompt,
        ) -> Result<mpsc::Receiver<Result<StreamDelta, PromptError>>, PromptError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(self.err.clone())
        }
    }

    fn entry(model_id: &str, provider: Arc<dyn LlmProvider>) -> ChainEntry {
        ChainEntry {
            model_id: model_id.to_string(),
            max_tokens: None,
            provider,
        }
    }

    #[tokio::test]
    async fn primary_succeeds_returns_immediately() {
        let chain = vec![entry("primary", Arc::new(OkProvider { name: "anthropic" }))];
        let outcome = walk(&chain, &Prompt::default()).await.unwrap();
        assert_eq!(outcome.model_used, "primary");
        assert_eq!(outcome.attempts.len(), 1);
        assert!(matches!(
            outcome.attempts[0].outcome,
            AttemptOutcome::Succeeded
        ));
    }

    #[tokio::test]
    async fn transient_falls_back() {
        let bad = Arc::new(FailingProvider::new(
            "bad",
            PromptError::transient(TransientKind::ServerError, "503"),
        ));
        let bad_calls = bad.clone();
        let good = Arc::new(OkProvider { name: "openai" });
        let chain = vec![entry("primary", bad), entry("fallback", good)];
        let outcome = walk(&chain, &Prompt::default()).await.unwrap();
        assert_eq!(outcome.model_used, "fallback");
        assert_eq!(outcome.attempts.len(), 2);
        assert!(matches!(
            outcome.attempts[0].outcome,
            AttemptOutcome::Transient(_)
        ));
        assert!(matches!(
            outcome.attempts[1].outcome,
            AttemptOutcome::Succeeded
        ));
        assert_eq!(bad_calls.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn terminal_aborts_chain_without_trying_fallback() {
        let bad = Arc::new(FailingProvider::new(
            "auth-bad",
            PromptError::terminal(TerminalKind::Auth, "401"),
        ));
        let good = Arc::new(FailingProvider::new(
            "never-called",
            PromptError::transient(TransientKind::ServerError, "503"),
        ));
        let never_calls = good.clone();
        let chain = vec![entry("primary", bad), entry("fallback", good)];
        let err = walk(&chain, &Prompt::default()).await.unwrap_err();
        assert!(err.is_terminal());
        assert_eq!(never_calls.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn empty_chain_errors_cleanly() {
        let chain: Vec<ChainEntry> = vec![];
        let err = walk(&chain, &Prompt::default()).await.unwrap_err();
        assert!(matches!(err, PromptError::Provider(_)));
    }
}
