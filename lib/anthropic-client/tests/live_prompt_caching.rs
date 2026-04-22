use std::env;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anthropic_client::AnthropicClient;
use llm::prompt::{CacheTtl, Message, SystemBlock, UserBlock};
use llm::Prompt;

const LIVE_TESTS_ENV: &str = "DRUA_LIVE_CACHE_TESTS";
const API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const MODEL_ENV: &str = "DRUA_ANTHROPIC_CACHE_TEST_MODEL";
const DEFAULT_MODEL: &str = "claude-haiku-4-5-20251001";
const MIN_CACHEABLE_TOKENS: usize = 6000;

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

fn build_prompt(model: &str, system_text: String, user_text: impl Into<String>) -> Prompt {
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
        cache_key: None,
    }
}

fn build_large_system_prefix(model: &str, nonce: &str) -> String {
    let mut prefix = String::new();
    let mut line = 0usize;

    loop {
        line += 1;
        prefix.push_str(
            format!(
                "Anthropic prompt cache integration test nonce {nonce} line {line:04}. \
                 Preserve this exact prefix so prompt caching can reuse the \
                 same prompt prefix across follow-up requests.\n"
            )
            .as_str(),
        );

        let probe = build_prompt(model, prefix.clone(), "Reply with the word: probe");
        if probe.estimate_tokens() >= MIN_CACHEABLE_TOKENS {
            return prefix;
        }
    }
}

#[tokio::test]
async fn anthropic_reports_cache_write_then_cache_read_on_follow_up_prompt() {
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
    let system_text = build_large_system_prefix(&model, &nonce);
    let client = AnthropicClient::new(api_key);

    let mut warm_prompt = build_prompt(
        &model,
        system_text.clone(),
        "Reply with the single word: seeded",
    );
    assert!(
        warm_prompt.enable_anthropic_prompt_caching(Some(CacheTtl::FiveMinutes)),
        "expected prompt caching marker to be applied"
    );
    let warm_response = client
        .send_prompt(&warm_prompt)
        .await
        .expect("warm request should succeed");
    assert!(
        !warm_response.content.is_empty(),
        "expected content from warm request"
    );
    assert!(
        warm_response.usage.cache_creation_input_tokens > 0,
        "expected warm request to create a cache entry, got {:?}",
        warm_response.usage
    );

    let mut cache_reads = Vec::new();
    for attempt in 0..3 {
        let mut prompt = build_prompt(
            &model,
            system_text.clone(),
            format!("Reply with the single word: warmed-{attempt}"),
        );
        assert!(
            prompt.enable_anthropic_prompt_caching(Some(CacheTtl::FiveMinutes)),
            "expected prompt caching marker to be applied"
        );

        let response = client
            .send_prompt(&prompt)
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
