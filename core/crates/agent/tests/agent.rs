use agent::{AgentMessageEvent, AgentRole, Agents};
use llm::{PromptRequest, PromptResponseEvent};
use primitives::{AuthSubject, UserId, WorkspaceId};
use tokio::sync::mpsc;

const PG_CON: &str = "postgres://user:password@localhost:5432/galoy_agents";

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| PG_CON.to_string());
    sqlx::PgPool::connect(&url).await.expect("connect to pg")
}

#[tokio::test]
async fn send_message_round_trip_via_prompt_channel() {
    let pool = pool().await;

    let (prompt_tx, mut prompt_rx) = mpsc::channel::<PromptRequest>(64);
    let agents = Agents::new(&pool, prompt_tx);

    let agent = agents
        .create(WorkspaceId::new(), AgentRole::WorkspaceLead, "lead")
        .await
        .expect("create agent");

    let mut events_rx = agents
        .send_message(
            AuthSubject::User(UserId::new()),
            agent.id,
            "Hello agent".to_string(),
        )
        .await
        .expect("send_message");

    // Evaluator side: receive the dispatched prompt request, send one Text
    // response and then drop the response channel so the forwarder loop
    // terminates on its own.
    let request = prompt_rx.recv().await.expect("prompt request dispatched");
    request
        .response_channel
        .send(PromptResponseEvent::Text {
            text: "Hi user".to_string(),
        })
        .await
        .expect("send text response");
    drop(request);

    // Drain the AgentMessageEvent channel until the forwarder closes it.
    let mut events = Vec::new();
    while let Some(event) = events_rx.recv().await {
        events.push(event);
    }

    assert_eq!(
        events.len(),
        2,
        "expected user echo + assistant text, got {events:?}"
    );

    match &events[0] {
        AgentMessageEvent::UserMessage { text, .. } => assert_eq!(text, "Hello agent"),
        other => panic!("event[0] should be UserMessage, got {other:?}"),
    }
    match &events[1] {
        AgentMessageEvent::AssistantText { text } => assert_eq!(text, "Hi user"),
        other => panic!("event[1] should be AssistantText, got {other:?}"),
    }
}
