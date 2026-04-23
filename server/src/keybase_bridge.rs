use drua_core::primitives::{AuthSubject, ChatOutputEvent};
use keybase_client::{ChatMessage, KeybaseClient, KeybaseClientError};

use drua_core::App;

#[derive(thiserror::Error, Debug)]
pub enum KeybaseBridgeError {
    #[error("KeybaseBridgeError - Keybase: {0}")]
    Keybase(#[from] KeybaseClientError),
    #[error("KeybaseBridgeError - Domain: {0}")]
    Domain(String),
}

/// Run the Keybase chat bridge.
///
/// Logs in the bot, listens for incoming chat events, routes messages to the
/// matching workspace's lead agent, and sends the agent's response back as a
/// threaded reply.
pub async fn run(
    app: App,
    bot_username: String,
    paperkey: String,
    keybase_path: String,
) -> Result<(), KeybaseBridgeError> {
    let client = KeybaseClient::new(&keybase_path);
    client.login(&bot_username, &paperkey).await?;

    let mut rx = client.listen()?;
    tracing::info!("Keybase bridge listening for chat events");

    while let Some(event) = rx.recv().await {
        let event = match event {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "Keybase listener error, skipping event");
                continue;
            }
        };

        // Only handle text messages.
        if event.type_ != "chat" {
            continue;
        }
        let msg = match event.msg {
            Some(m) => m,
            None => continue,
        };
        if msg.content.type_ != "text" {
            continue;
        }
        // Skip messages from the bot itself.
        if msg.sender.username == bot_username {
            continue;
        }
        let text = match msg.content.text {
            Some(ref t) if !t.body.trim().is_empty() => t.body.clone(),
            _ => continue,
        };

        let app = app.clone();
        let client = client.clone();
        let bot_username = bot_username.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_message(&app, &client, &bot_username, &msg, &text).await {
                tracing::error!(
                    error = %e,
                    channel = %msg.channel.name,
                    sender = %msg.sender.username,
                    "Failed to handle Keybase message"
                );
            }
        });
    }

    tracing::warn!("Keybase listener stream ended");
    Ok(())
}

async fn handle_message(
    app: &App,
    client: &KeybaseClient,
    _bot_username: &str,
    msg: &ChatMessage,
    text: &str,
) -> Result<(), KeybaseBridgeError> {
    let sub = AuthSubject::Keybase(msg.sender.username.clone());

    // Find the workspace matching the incoming channel's team + topic.
    let workspaces = app
        .workspaces()
        .list_all(&sub)
        .await
        .map_err(|e| KeybaseBridgeError::Domain(e.to_string()))?;

    let topic = msg.channel.topic_name.as_deref().unwrap_or("");
    let workspace = workspaces.into_iter().find(|ws| {
        ws.keybase_team.as_deref() == Some(&msg.channel.name)
            && ws.keybase_channel.as_deref() == Some(topic)
    });

    let workspace = match workspace {
        Some(ws) => ws,
        None => {
            tracing::debug!(
                team = %msg.channel.name,
                topic = topic,
                "No workspace matched Keybase channel, ignoring message"
            );
            return Ok(());
        }
    };

    // Send to the workspace's lead agent.
    let mut rx = app
        .agents()
        .send_message(sub, workspace.lead_agent_id, text.to_string())
        .await
        .map_err(|e| KeybaseBridgeError::Domain(e.to_string()))?;

    // Collect the agent's text response.
    let mut response = String::new();
    while let Some(event) = rx.recv().await {
        match event {
            ChatOutputEvent::TextDelta { text } => response.push_str(&text),
            ChatOutputEvent::AssistantText { text } => response.push_str(&text),
            ChatOutputEvent::AssistantDone { .. } => break,
            ChatOutputEvent::Error { message } => {
                tracing::error!(error = %message, "Agent returned error");
                response = format!("⚠️ Error: {message}");
                break;
            }
            _ => {}
        }
    }

    if response.is_empty() {
        return Ok(());
    }

    client.send_reply(&msg.channel, &response, msg.id).await?;

    Ok(())
}
