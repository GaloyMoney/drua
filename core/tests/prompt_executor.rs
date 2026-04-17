use galoy_agents_core::prompt_executor::{
    ModelConfig, PromptExecutor, PromptExecutorConfig, Provider,
};
use llm::prompt::{Message, UserBlock};
use llm::{Prompt, PromptRequest};

const MODEL: &str = "claude-haiku-4-5-20251001";

#[tokio::test]
async fn anthropic_round_trip_via_executor() {
    let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") else {
        eprintln!("ANTHROPIC_API_KEY not set, skipping anthropic_round_trip_via_executor");
        return;
    };

    let config = PromptExecutorConfig {
        models: vec![ModelConfig {
            name: MODEL.to_string(),
            provider: Provider::Anthropic { api_key },
            default_max_tokens: Some(64),
        }],
    };

    let (_executor, prompt_tx) = PromptExecutor::init(config).await;

    let prompt = Prompt {
        model: MODEL.to_string(),
        messages: vec![Message::User {
            content: vec![UserBlock::Text {
                text: "Reply with the single word: pong".to_string(),
                cache_control: None,
            }],
        }],
        system: Vec::new(),
        tools: Vec::new(),
        tool_choice: None,
        max_tokens: None, // executor should fill this in from default_max_tokens
    };

    let (request, response_rx) = PromptRequest::new(prompt);
    prompt_tx.send(request).await.expect("dispatch request");

    let result = response_rx
        .await
        .expect("response channel closed without sending")
        .expect("anthropic returned an error");

    assert!(
        !result.content.is_empty(),
        "expected at least one content block in the response"
    );
    assert!(
        result.usage.input_tokens > 0,
        "expected non-zero input_tokens, got {:?}",
        result.usage
    );
    assert!(
        result.usage.output_tokens > 0,
        "expected non-zero output_tokens, got {:?}",
        result.usage
    );
}
