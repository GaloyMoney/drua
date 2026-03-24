use std::collections::HashMap;
use std::sync::Arc;

use style_agent_core::label_store::{load_audit_suspects, LabelStore};
use style_agent_core::review_session::{
    compute_label_counts, compute_saturated_labels, persist_confirm, persist_confirm_label_only,
    persist_undo, persist_undo_label_only, review_chunks_from_suspects, ReviewSession,
};
use style_agent_core::store::{ReviewChunk, VectorStore, KNOWN_PRIMARY_LABELS};
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode};
use tokio::sync::Mutex;

use crate::config::Config;

/// Maximum characters of code to display before truncating.
const MAX_CODE_CHARS: usize = 3000;
/// Maximum lines of code to display.
const MAX_CODE_LINES: usize = 50;

type HandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// Shared bot state injected into handlers.
#[derive(Clone)]
struct BotState {
    sessions: Arc<Mutex<HashMap<ChatId, ReviewSession>>>,
    store: Arc<VectorStore>,
    label_store: Arc<LabelStore>,
    saturated_labels: std::collections::HashSet<String>,
    label_counts: std::collections::HashMap<String, usize>,
    target_per_label: usize,
    allowed_user_ids: Vec<u64>,
    /// When true, chunks come from audit suspects and persist is label-only.
    confused: bool,
    /// Pre-loaded confused chunks (empty when `confused` is false).
    confused_chunks: Vec<ReviewChunk>,
}

pub async fn run(config: &Config, confused: bool) -> anyhow::Result<()> {
    let tg_config = config
        .telegram
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Missing [telegram] section in config"))?;

    let bot_token = std::env::var(&tg_config.bot_token_env).map_err(|_| {
        anyhow::anyhow!("Environment variable '{}' not set", tg_config.bot_token_env)
    })?;

    let db_path = config.db_path();
    let store = VectorStore::new(&db_path)?;
    store.ensure_collection()?;
    let label_store = LabelStore::new(config.labels_dir().join("labels.jsonl"));

    let min_per_label = config.services.min_reviews_per_label;
    let label_counts = compute_label_counts(&label_store);
    let saturated_labels = compute_saturated_labels(&label_store, min_per_label);
    if !saturated_labels.is_empty() {
        tracing::info!(
            "Skipping saturated labels (>={min_per_label}): {:?}",
            saturated_labels
        );
    }

    let confused_chunks = if confused {
        let suspects_path = config.labels_dir().join("audit-suspects.jsonl");
        let suspects = load_audit_suspects(&suspects_path)?;
        if suspects.is_empty() {
            anyhow::bail!("No audit suspects found. Run `make audit-labels` first.");
        }
        tracing::info!("Loaded {} audit suspects for re-review", suspects.len());
        review_chunks_from_suspects(&suspects)
    } else {
        Vec::new()
    };

    let bot = Bot::new(bot_token);
    let state = BotState {
        sessions: Arc::new(Mutex::new(HashMap::new())),
        store: Arc::new(store),
        label_store: Arc::new(label_store),
        saturated_labels,
        label_counts,
        target_per_label: min_per_label,
        allowed_user_ids: tg_config.allowed_user_ids.clone(),
        confused,
        confused_chunks,
    };

    tracing::info!("Starting Telegram review bot...");

    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(handle_message))
        .branch(Update::filter_callback_query().endpoint(handle_callback));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state])
        .build()
        .dispatch()
        .await;

    Ok(())
}

fn is_authorized(user_id: u64, allowed: &[u64]) -> bool {
    allowed.is_empty() || allowed.contains(&user_id)
}

async fn handle_message(bot: Bot, msg: Message, state: BotState) -> HandlerResult {
    let user_id = match msg.from {
        Some(ref u) => u.id.0,
        None => return Ok(()),
    };
    if !is_authorized(user_id, &state.allowed_user_ids) {
        return Ok(());
    }

    let text = msg.text().unwrap_or("");
    if !text.starts_with("/start") {
        return Ok(());
    }

    bot.send_message(msg.chat.id, "Loading chunks...").await?;

    let chunks = if state.confused {
        if state.confused_chunks.is_empty() {
            bot.send_message(
                msg.chat.id,
                "No audit suspects found. Run `make audit-labels` first.",
            )
            .await?;
            return Ok(());
        }
        state.confused_chunks.clone()
    } else {
        match state.store.scroll_all_chunks() {
            Ok(c) => c,
            Err(e) => {
                bot.send_message(msg.chat.id, format!("Failed to load chunks: {e}"))
                    .await?;
                return Ok(());
            }
        }
    };

    if chunks.is_empty() {
        bot.send_message(msg.chat.id, "No chunks found. Run bootstrap first.")
            .await?;
        return Ok(());
    }

    let session = ReviewSession::new(
        chunks,
        state.saturated_labels.clone(),
        state.label_counts.clone(),
        state.target_per_label,
    );

    if session.is_done() {
        bot.send_message(msg.chat.id, "All chunks have been reviewed!")
            .await?;
        return Ok(());
    }

    let total = session.filtered_count();

    // Grab display data before inserting session.
    let first_chunk = session.current_chunk().unwrap().clone();
    let toggled = session.selected_labels().clone();

    state.sessions.lock().await.insert(msg.chat.id, session);

    let text = format_chunk(&first_chunk, 0, total);
    let keyboard = build_keyboard(&toggled);

    bot.send_message(msg.chat.id, text)
        .parse_mode(ParseMode::Html)
        .reply_markup(keyboard)
        .await?;

    Ok(())
}

async fn handle_callback(bot: Bot, q: CallbackQuery, state: BotState) -> HandlerResult {
    let user_id = q.from.id.0;
    if !is_authorized(user_id, &state.allowed_user_ids) {
        return Ok(());
    }

    let data = match q.data.as_deref() {
        Some(d) => d.to_string(),
        None => return Ok(()),
    };

    let (chat_id, message_id) = match q.regular_message() {
        Some(msg) => (msg.chat.id, msg.id),
        None => return Ok(()),
    };

    bot.answer_callback_query(q.id.clone()).await?;

    let mut sessions = state.sessions.lock().await;
    let session = match sessions.get_mut(&chat_id) {
        Some(s) => s,
        None => {
            drop(sessions);
            bot.send_message(chat_id, "No active session. Send /start to begin.")
                .await?;
            return Ok(());
        }
    };

    if let Some(label) = data.strip_prefix("toggle:") {
        session.toggle_label(label);
        let toggled = session.selected_labels().clone();
        drop(sessions);

        bot.edit_message_reply_markup(chat_id, message_id)
            .reply_markup(build_keyboard(&toggled))
            .await?;
    } else if data == "confirm" {
        if let Some(confirm_data) = session.confirm() {
            let total = session.total_count();
            let is_done = session.is_done();
            let summary_data = session.summary();

            // Grab next chunk display data before dropping the lock.
            let next_display = if !is_done {
                let chunk = session.current_chunk().unwrap().clone();
                let idx = session.current_index();
                let toggled = session.selected_labels().clone();
                Some((chunk, idx, total, toggled))
            } else {
                None
            };

            if is_done {
                sessions.remove(&chat_id);
            }
            drop(sessions);

            let persist_result = if state.confused {
                persist_confirm_label_only(&state.label_store, &confirm_data)
            } else {
                persist_confirm(&state.store, &state.label_store, &confirm_data)
            };
            if let Err(e) = persist_result {
                bot.send_message(chat_id, format!("Failed to save: {e}"))
                    .await?;
                return Ok(());
            }

            if let Some((chunk, idx, total, toggled)) = next_display {
                let text = format_chunk(&chunk, idx, total);
                bot.send_message(chat_id, text)
                    .parse_mode(ParseMode::Html)
                    .reply_markup(build_keyboard(&toggled))
                    .await?;
            } else {
                let summary =
                    format_summary(summary_data.reviewed, summary_data.skipped, "complete");
                bot.send_message(chat_id, summary).await?;
            }
        }
    } else if data == "skip" {
        session.skip();
        let is_done = session.is_done();
        let summary_data = session.summary();

        let next_display = if !is_done {
            let chunk = session.current_chunk().unwrap().clone();
            let idx = session.current_index();
            let total = session.total_count();
            let toggled = session.selected_labels().clone();
            Some((chunk, idx, total, toggled))
        } else {
            None
        };

        if is_done {
            sessions.remove(&chat_id);
        }
        drop(sessions);

        if let Some((chunk, idx, total, toggled)) = next_display {
            let text = format_chunk(&chunk, idx, total);
            bot.send_message(chat_id, text)
                .parse_mode(ParseMode::Html)
                .reply_markup(build_keyboard(&toggled))
                .await?;
        } else {
            let summary = format_summary(summary_data.reviewed, summary_data.skipped, "complete");
            bot.send_message(chat_id, summary).await?;
        }
    } else if data == "undo" {
        if let Some(undo_data) = session.undo() {
            let chunk = session.current_chunk().unwrap().clone();
            let idx = session.current_index();
            let total = session.total_count();
            let toggled = session.selected_labels().clone();
            drop(sessions);

            let undo_result = if state.confused {
                persist_undo_label_only(&state.label_store)
            } else {
                persist_undo(&state.store, &state.label_store, &undo_data)
            };
            if let Err(e) = undo_result {
                bot.send_message(chat_id, format!("Undo failed: {e}"))
                    .await?;
                return Ok(());
            }

            let text = format_chunk(&chunk, idx, total);
            bot.send_message(chat_id, text)
                .parse_mode(ParseMode::Html)
                .reply_markup(build_keyboard(&toggled))
                .await?;
        } else {
            drop(sessions);
            bot.send_message(chat_id, "Nothing to undo.").await?;
        }
    } else if data == "stop" {
        let summary_data = session.summary();
        sessions.remove(&chat_id);
        drop(sessions);

        let summary = format_summary(summary_data.reviewed, summary_data.skipped, "stopped");
        bot.send_message(chat_id, summary).await?;
    }

    Ok(())
}

fn format_summary(confirmed: usize, skipped: usize, reason: &str) -> String {
    format!(
        "Review {reason}!\n\nConfirmed: {confirmed}\nSkipped: {skipped}\nTotal reviewed: {}",
        confirmed + skipped,
    )
}

fn format_chunk(chunk: &ReviewChunk, index: usize, total: usize) -> String {
    let entity_line = chunk
        .entity_name
        .as_deref()
        .map(|e| format!(" | Entity: {e}"))
        .unwrap_or_default();

    let labels_str = if chunk.labels.is_empty() {
        "none".to_string()
    } else {
        chunk.labels.join(", ")
    };

    let code = truncate_code(&chunk.content);
    let escaped_code = html_escape(&code);

    format!(
        "Chunk {idx}/{total} | repo: {repo}\n\
         {path}\n\
         Type: {ctype}{entity}\n\n\
         <pre language=\"{lang}\">\n{code}\n</pre>\n\n\
         Current labels: {labels}",
        idx = index + 1,
        repo = html_escape(&chunk.repo),
        path = html_escape(&chunk.file_path),
        ctype = html_escape(&chunk.chunk_type),
        entity = html_escape(&entity_line),
        lang = chunk.language,
        code = escaped_code,
        labels = html_escape(&labels_str),
    )
}

fn truncate_code(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    if content.len() <= MAX_CODE_CHARS && total_lines <= MAX_CODE_LINES {
        return content.to_string();
    }

    let take_lines = MAX_CODE_LINES.min(total_lines);
    let mut result: String = lines[..take_lines].join("\n");

    if result.len() > MAX_CODE_CHARS {
        result.truncate(MAX_CODE_CHARS);
    }

    if take_lines < total_lines || result.len() < content.len() {
        result.push_str(&format!("\n... [truncated, {total_lines} lines total]"));
    }

    result
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn build_keyboard(toggled: &std::collections::HashSet<String>) -> InlineKeyboardMarkup {
    let visible_labels: Vec<&str> = KNOWN_PRIMARY_LABELS.to_vec();

    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    let mut current_row: Vec<InlineKeyboardButton> = Vec::new();

    for label in &visible_labels {
        let is_on = toggled.contains(*label);
        let display = if is_on {
            format!("\u{2713} {label}")
        } else {
            label.to_string()
        };
        current_row.push(InlineKeyboardButton::callback(
            display,
            format!("toggle:{label}"),
        ));
        if current_row.len() == 3 {
            rows.push(std::mem::take(&mut current_row));
        }
    }
    if !current_row.is_empty() {
        rows.push(current_row);
    }

    rows.push(vec![
        InlineKeyboardButton::callback("Confirm", "confirm"),
        InlineKeyboardButton::callback("Skip", "skip"),
        InlineKeyboardButton::callback("Undo", "undo"),
        InlineKeyboardButton::callback("Stop", "stop"),
    ]);

    InlineKeyboardMarkup::new(rows)
}
