//! Live smoke test against the real OpenRouter decisions endpoint. Skips
//! unless `OPENROUTER_API_KEY` is set (pattern:
//! `lib/openai-client/tests/live_prompt_caching.rs:11`) — never set this in
//! CI or during development; it bills real inference.

use std::collections::BTreeMap;
use std::env;
use std::time::Duration;

use decision_client::{DecisionClient, DecisionLimits, DecisionRequest, Question};

const API_KEY_ENV: &str = "OPENROUTER_API_KEY";
const ENDPOINT_URL: &str = "https://openrouter.ai/api/alpha/decisions";
const MODEL: &str = "typesafe/jev-1.13";

fn maybe_load_dotenv() {
    let _ = dotenvy::dotenv();
}

#[tokio::test]
async fn decide_against_live_openrouter_returns_calibrated_answers() {
    maybe_load_dotenv();

    let Ok(api_key) = env::var(API_KEY_ENV) else {
        eprintln!("{API_KEY_ENV} not set, skipping live decide test");
        return;
    };

    let client = DecisionClient::new(
        api_key,
        ENDPOINT_URL,
        Duration::from_secs(30),
        DecisionLimits::default(),
    );

    let mut criteria = BTreeMap::new();
    criteria.insert("positive".to_string(), "An upbeat, positive tone".into());
    criteria.insert("negative".to_string(), "A downbeat, negative tone".into());

    let mut questions = BTreeMap::new();
    questions.insert(
        "tone".to_string(),
        Question::Choice {
            instructions: "What is the tone of `state`?".into(),
            criteria,
        },
    );
    questions.insert(
        "is_about_weather".to_string(),
        Question::Noul {
            instructions: "Is `state` about the weather?".into(),
            criteria: None,
        },
    );

    let request = DecisionRequest {
        model: MODEL.to_string(),
        state: serde_json::json!("What a gorgeous, sunny morning! The sky is perfectly clear."),
        questions,
    };

    let response = client.decide(&request).await.expect("live decide call");
    assert!(response.answers.contains_key("tone"));
    assert!(response.answers.contains_key("is_about_weather"));
    assert!(
        response.usage.cost_usd.is_some(),
        "OpenRouter should report usage.cost"
    );
}
