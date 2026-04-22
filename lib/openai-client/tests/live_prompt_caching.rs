use std::env;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use llm::prompt::{Message, SystemBlock, UserBlock};
use llm::provider::LlmProvider;
use llm::stream::StreamAccumulator;
use llm::{Prompt, PromptResponse};
use openai_client::{OpenAiResponsesAuth, OpenAiResponsesClient};

const LIVE_TESTS_ENV: &str = "DRUA_LIVE_CACHE_TESTS";
const API_KEY_ENV: &str = "OPENAI_API_KEY";
const MODEL_ENV: &str = "DRUA_OPENAI_CACHE_TEST_MODEL";
const DEFAULT_MODEL: &str = "gpt-5.4-mini";
const MIN_CACHEABLE_TOKENS: usize = 1400;

fn env_flag(name: &str) -> bool {
    env::var(name).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        )
    })
}

fn maybe_load_dotenv() {
    let _ = dotenvy::dotenv();
}

fn unique_nonce() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{timestamp}", std::process::id())
}

fn build_prompt(
    model: &str,
    cache_key: &str,
    system_text: String,
    user_text: impl Into<String>,
) -> Prompt {
    Prompt {
        model: model.to_string(),
        messages: vec![Message::User {
            content: vec![UserBlock::Text {
                text: user_text.into(),
                cache_control: None,
            }],
        }],
        system: vec![SystemBlock::Text {
            text: system_text,
            cache_control: None,
        }],
        tools: Vec::new(),
        tool_choice: None,
        max_tokens: Some(64),
        cache_key: Some(cache_key.to_string()),
    }
}

fn build_large_system_prefix(model: &str, nonce: &str) -> String {
    let mut prefix = String::new();
    let mut line = 0usize;

    loop {
        line += 1;
        prefix.push_str(
            format!(
                "Prompt cache integration test nonce {nonce} line {line:04}. \
                 Reuse this exact prefix across requests so the provider can \
                 skip rebuilding the shared KV state.\n"
            )
            .as_str(),
        );

        let probe = build_prompt(model, "probe", prefix.clone(), "Reply with the word: probe");
        if probe.estimate_tokens() >= MIN_CACHEABLE_TOKENS {
            return prefix;
        }
    }
}

async fn send_prompt(
    client: &OpenAiResponsesClient,
    prompt: &Prompt,
) -> Result<PromptResponse, String> {
    let mut rx = client
        .send_prompt_streaming(prompt)
        .await
        .map_err(|err| err.to_string())?;

    let mut accumulator = StreamAccumulator::new();
    while let Some(event) = rx.recv().await {
        let delta = event.map_err(|err| err.to_string())?;
        accumulator.process(&delta);
    }

    Ok(accumulator.finish())
}

#[tokio::test]
async fn openai_responses_reports_cached_tokens_on_follow_up_prompt() {
    maybe_load_dotenv();

    if !env_flag(LIVE_TESTS_ENV) {
        eprintln!("Set {LIVE_TESTS_ENV}=1 to run live prompt caching tests");
        return;
    }

    let Ok(api_key) = env::var(API_KEY_ENV) else {
        eprintln!("{API_KEY_ENV} not set, skipping live prompt caching test");
        return;
    };
    let model = env::var(MODEL_ENV).unwrap_or_else(|_| DEFAULT_MODEL.to_string());

    let nonce = unique_nonce();
    let cache_key = format!("drua-live-cache-test:{nonce}");
    let system_text = build_large_system_prefix(&model, &nonce);

    let client = OpenAiResponsesClient::new(OpenAiResponsesAuth::ApiKey { api_key });

    let warm_prompt = build_prompt(
        &model,
        &cache_key,
        system_text.clone(),
        "Reply with the single word: seeded",
    );
    let warm_response = send_prompt(&client, &warm_prompt)
        .await
        .expect("warm request should succeed");
    assert!(
        !warm_response.content.is_empty(),
        "expected content from warm request"
    );

    let mut cache_reads = Vec::new();
    for attempt in 0..3 {
        let prompt = build_prompt(
            &model,
            &cache_key,
            system_text.clone(),
            format!("Reply with the single word: warmed-{attempt}"),
        );
        let response = send_prompt(&client, &prompt)
            .await
            .expect("follow-up request should succeed");
        cache_reads.push(response.usage.cache_read_input_tokens);
        if response.usage.cache_read_input_tokens > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    panic!(
        "expected cached input tokens on a warmed follow-up prompt, got {:?}",
        cache_reads
    );
}
