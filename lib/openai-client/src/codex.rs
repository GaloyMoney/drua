use std::collections::HashMap;

use async_trait::async_trait;
use base64::Engine as _;
use llm::prompt::{
    AssistantBlock, Message, SystemBlock, Tool, ToolChoice, ToolResultBlock, UserBlock,
};
use llm::provider::LlmProvider;
use llm::stream::StreamDelta;
use llm::{Prompt, PromptError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::instrument;

use crate::sse::{parse_sse_stream, SseError};
use crate::OpenAiError;

const DEFAULT_CODEX_API_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4096;
const DEFAULT_REASONING_EFFORT: &str = "low";
const DEFAULT_TEXT_VERBOSITY: &str = "medium";
const OPENAI_CODEX_ACCESS_TOKEN_ENV: &str = "OPENAI_CODEX_ACCESS_TOKEN";

#[derive(Clone)]
pub struct OpenAiCodexClient {
    http: reqwest::Client,
    api_url: String,
}

impl OpenAiCodexClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            api_url: DEFAULT_CODEX_API_URL.to_string(),
        }
    }

    #[instrument(name = "openai_codex_client.send_prompt_streaming", skip_all)]
    async fn send_prompt_streaming_internal(
        &self,
        prompt: &Prompt,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<StreamDelta, OpenAiError>>, OpenAiError> {
        let access_token = resolve_codex_access_token().ok_or(OpenAiError::Api {
            status: 401,
            message: format!(
                "OpenAI Codex credentials not found. Run `codex login` or set {}.",
                OPENAI_CODEX_ACCESS_TOKEN_ENV
            ),
        })?;
        let account_id = extract_chatgpt_account_id(&access_token).ok_or(OpenAiError::Api {
            status: 401,
            message:
                "OpenAI Codex credential is invalid or expired (missing chatgpt_account_id claim)"
                    .to_string(),
        })?;

        let request_body = prompt_to_codex_request(prompt);

        let resp = self
            .http
            .post(&self.api_url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("chatgpt-account-id", account_id)
            .header("OpenAI-Beta", "responses=experimental")
            .header("originator", "drua")
            .header("User-Agent", "drua")
            .header("accept", "text/event-stream")
            .header("content-type", "application/json")
            .json(&request_body)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let message = resp.text().await.unwrap_or_default();
            return Err(OpenAiError::Api {
                status: status.as_u16(),
                message,
            });
        }

        let byte_stream = resp.bytes_stream();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<StreamDelta, OpenAiError>>(128);

        tokio::spawn(async move {
            let tx_ref = &tx;
            let mut synthesizer = CodexDeltaSynthesizer::new();
            let synth_ref = &mut synthesizer;

            let parse_result = parse_sse_stream(byte_stream, |event| {
                match synth_ref.process_event(&event.data) {
                    Ok(deltas) => {
                        for delta in deltas {
                            tx_ref
                                .try_send(Ok(delta))
                                .map_err(|e| SseError::Processing(e.to_string()))?;
                        }
                        Ok(())
                    }
                    Err(e) => {
                        let _ = tx_ref.try_send(Err(OpenAiError::Stream(e.clone())));
                        Err(SseError::Processing(e))
                    }
                }
            })
            .await;

            if let Ok(deltas) = synth_ref.finish_stream() {
                for delta in deltas {
                    if tx_ref.try_send(Ok(delta)).is_err() {
                        return;
                    }
                }
            }

            if let Err(e) = parse_result {
                let _ = tx_ref.try_send(Err(OpenAiError::from(e)));
            }
        });

        Ok(rx)
    }
}

impl Default for OpenAiCodexClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LlmProvider for OpenAiCodexClient {
    fn name(&self) -> &str {
        "openai-codex"
    }

    async fn send_prompt_streaming(
        &self,
        prompt: &Prompt,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<StreamDelta, PromptError>>, PromptError> {
        let rx = self
            .send_prompt_streaming_internal(prompt)
            .await
            .map_err(|e| PromptError::Provider(e.to_string()))?;

        let (tx, out_rx) = tokio::sync::mpsc::channel(128);
        tokio::spawn(async move {
            let mut rx = rx;
            while let Some(result) = rx.recv().await {
                let mapped = result.map_err(|e| PromptError::Provider(e.to_string()));
                if tx.send(mapped).await.is_err() {
                    break;
                }
            }
        });
        Ok(out_rx)
    }
}

#[derive(Debug, Serialize)]
struct CodexRequest {
    model: String,
    input: Vec<CodexInputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<CodexTool>>,
    tool_choice: Value,
    parallel_tool_calls: bool,
    stream: bool,
    store: bool,
    text: CodexTextConfig,
    include: Vec<&'static str>,
    reasoning: CodexReasoningConfig,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CodexInputItem {
    Message {
        role: String,
        content: Vec<CodexMessageContent>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CodexMessageContent {
    InputText { text: String },
    OutputText { text: String },
}

#[derive(Debug, Serialize)]
struct CodexTool {
    #[serde(rename = "type")]
    kind: &'static str,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: Value,
}

#[derive(Debug, Serialize)]
struct CodexTextConfig {
    verbosity: &'static str,
}

#[derive(Debug, Serialize)]
struct CodexReasoningConfig {
    effort: &'static str,
    summary: &'static str,
}

fn prompt_to_codex_request(prompt: &Prompt) -> CodexRequest {
    let mut input = Vec::new();

    for message in &prompt.messages {
        convert_message(message, &mut input);
    }

    let tools = if prompt.tools.is_empty() {
        None
    } else {
        Some(prompt.tools.iter().map(convert_tool).collect())
    };

    let instructions = prompt
        .system
        .iter()
        .map(|block| match block {
            SystemBlock::Text { text, .. } => text.as_str(),
        })
        .collect::<Vec<_>>()
        .join("\n");

    CodexRequest {
        model: prompt.model.clone(),
        input,
        instructions: (!instructions.is_empty()).then_some(instructions),
        max_output_tokens: prompt.max_tokens.or(Some(DEFAULT_MAX_OUTPUT_TOKENS)),
        tools,
        tool_choice: convert_tool_choice(prompt.tool_choice.as_ref()),
        parallel_tool_calls: true,
        stream: true,
        store: false,
        text: CodexTextConfig {
            verbosity: DEFAULT_TEXT_VERBOSITY,
        },
        include: vec!["reasoning.encrypted_content"],
        reasoning: CodexReasoningConfig {
            effort: DEFAULT_REASONING_EFFORT,
            summary: "auto",
        },
    }
}

fn convert_message(message: &Message, out: &mut Vec<CodexInputItem>) {
    match message {
        Message::User { content } => {
            let mut text_parts = Vec::new();

            for block in content {
                match block {
                    UserBlock::Text { text, .. } => text_parts.push(text.clone()),
                    UserBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } => {
                        if !text_parts.is_empty() {
                            out.push(CodexInputItem::Message {
                                role: "user".to_string(),
                                content: vec![CodexMessageContent::InputText {
                                    text: text_parts.join("\n"),
                                }],
                            });
                            text_parts.clear();
                        }

                        let output = content
                            .iter()
                            .map(|block| match block {
                                ToolResultBlock::Text { text } => text.as_str(),
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        let output = if *is_error {
                            format!("ERROR: {output}")
                        } else {
                            output
                        };

                        out.push(CodexInputItem::FunctionCallOutput {
                            call_id: tool_use_id.clone(),
                            output,
                        });
                    }
                }
            }

            if !text_parts.is_empty() {
                out.push(CodexInputItem::Message {
                    role: "user".to_string(),
                    content: vec![CodexMessageContent::InputText {
                        text: text_parts.join("\n"),
                    }],
                });
            }
        }
        Message::Assistant { content } => {
            let mut text_parts = Vec::new();
            let mut tool_calls = Vec::new();

            for block in content {
                match block {
                    AssistantBlock::Text { text, .. } => text_parts.push(text.clone()),
                    AssistantBlock::ToolUse {
                        id, name, input, ..
                    } => {
                        tool_calls.push(CodexInputItem::FunctionCall {
                            call_id: id.clone(),
                            name: name.clone(),
                            arguments: serde_json::to_string(input).unwrap_or_default(),
                        });
                    }
                    AssistantBlock::Thinking { .. } => {}
                }
            }

            if !text_parts.is_empty() {
                out.push(CodexInputItem::Message {
                    role: "assistant".to_string(),
                    content: vec![CodexMessageContent::OutputText {
                        text: text_parts.join("\n"),
                    }],
                });
            }

            out.extend(tool_calls);
        }
    }
}

fn convert_tool(tool: &Tool) -> CodexTool {
    CodexTool {
        kind: "function",
        name: tool.name.clone(),
        description: tool.description.clone(),
        parameters: tool.input_schema.clone(),
    }
}

fn convert_tool_choice(choice: Option<&ToolChoice>) -> Value {
    match choice {
        Some(ToolChoice::Auto) | None => Value::String("auto".to_string()),
        Some(ToolChoice::Any) => Value::String("required".to_string()),
        Some(ToolChoice::None) => Value::String("none".to_string()),
        Some(ToolChoice::Tool { name }) => serde_json::json!({
            "type": "function",
            "name": name,
        }),
    }
}

#[derive(Default)]
struct CodexDeltaSynthesizer {
    text: String,
    thinking: String,
    tool_calls: HashMap<String, ToolCallState>,
    pending_usage: Option<(u32, u32)>,
    pending_incomplete_reason: Option<String>,
    saw_tool_call: bool,
    terminal_emitted: bool,
}

#[derive(Default)]
struct ToolCallState {
    call_id: String,
    name: String,
    arguments: String,
    started: bool,
}

impl CodexDeltaSynthesizer {
    fn new() -> Self {
        Self::default()
    }

    fn process_event(&mut self, data: &str) -> Result<Vec<StreamDelta>, String> {
        if data.trim() == "[DONE]" {
            return self.drain_terminal();
        }

        let event: CodexResponseEvent =
            serde_json::from_str(data).map_err(|e| format!("JSON parse: {e}"))?;

        match event {
            CodexResponseEvent::OutputTextDelta { delta, .. } => Ok(self.append_text_delta(&delta)),
            CodexResponseEvent::OutputTextDone { text, .. } => Ok(self.reconcile_text(&text)),
            CodexResponseEvent::ReasoningTextDelta { delta, .. }
            | CodexResponseEvent::ReasoningSummaryTextDelta { delta, .. } => {
                Ok(self.append_thinking_delta(&delta))
            }
            CodexResponseEvent::ReasoningTextDone { text, .. }
            | CodexResponseEvent::ReasoningSummaryTextDone { text, .. } => {
                Ok(self.reconcile_thinking(&text))
            }
            CodexResponseEvent::OutputItemAdded { item }
            | CodexResponseEvent::OutputItemDone { item } => self.process_output_item(item),
            CodexResponseEvent::FunctionCallArgumentsDelta { item_id, delta } => {
                Ok(self.append_tool_args(&item_id, &delta))
            }
            CodexResponseEvent::ResponseCompleted { response }
            | CodexResponseEvent::ResponseDone { response }
            | CodexResponseEvent::ResponseIncomplete { response } => {
                self.pending_usage = response
                    .usage
                    .as_ref()
                    .map(|usage| (usage.input_tokens, usage.output_tokens));
                self.pending_incomplete_reason = response.incomplete_reason();
                Ok(Vec::new())
            }
            CodexResponseEvent::ResponseFailed { response } => Err(response.error_message()),
            CodexResponseEvent::Error { error } => Err(error.message),
            CodexResponseEvent::Other => Ok(Vec::new()),
        }
    }

    fn finish_stream(&mut self) -> Result<Vec<StreamDelta>, String> {
        self.drain_terminal()
    }

    fn append_text_delta(&mut self, delta: &str) -> Vec<StreamDelta> {
        if delta.is_empty() {
            return Vec::new();
        }
        self.text.push_str(delta);
        vec![StreamDelta::TextDelta {
            text: delta.to_string(),
        }]
    }

    fn reconcile_text(&mut self, full_text: &str) -> Vec<StreamDelta> {
        if full_text.is_empty() || full_text == self.text {
            return Vec::new();
        }
        if let Some(suffix) = full_text.strip_prefix(&self.text) {
            return self.append_text_delta(suffix);
        }
        if self.text.is_empty() {
            return self.append_text_delta(full_text);
        }
        Vec::new()
    }

    fn append_thinking_delta(&mut self, delta: &str) -> Vec<StreamDelta> {
        if delta.is_empty() {
            return Vec::new();
        }
        self.thinking.push_str(delta);
        vec![StreamDelta::ThinkingDelta {
            text: delta.to_string(),
        }]
    }

    fn reconcile_thinking(&mut self, full_text: &str) -> Vec<StreamDelta> {
        if full_text.is_empty() || full_text == self.thinking {
            return Vec::new();
        }
        if let Some(suffix) = full_text.strip_prefix(&self.thinking) {
            return self.append_thinking_delta(suffix);
        }
        if self.thinking.is_empty() {
            return self.append_thinking_delta(full_text);
        }
        Vec::new()
    }

    fn process_output_item(&mut self, item: CodexOutputItem) -> Result<Vec<StreamDelta>, String> {
        if item.kind != "function_call" {
            return Ok(Vec::new());
        }

        let item_id = item
            .id
            .unwrap_or_else(|| item.call_id.clone().unwrap_or_default());
        let call_id = item.call_id.unwrap_or_else(|| item_id.clone());
        let name = item.name.unwrap_or_default();
        let mut deltas = self.ensure_tool_started(&item_id, &call_id, &name);

        if let Some(arguments) = item.arguments {
            let suffix = self.reconcile_tool_args(&item_id, &arguments)?;
            deltas.extend(suffix);
        }

        Ok(deltas)
    }

    fn ensure_tool_started(
        &mut self,
        item_id: &str,
        call_id: &str,
        name: &str,
    ) -> Vec<StreamDelta> {
        let state = self.tool_calls.entry(item_id.to_string()).or_default();
        if state.call_id.is_empty() {
            state.call_id = call_id.to_string();
        }
        if state.name.is_empty() {
            state.name = name.to_string();
        }

        if state.started {
            return Vec::new();
        }

        state.started = true;
        self.saw_tool_call = true;

        let mut deltas = vec![StreamDelta::ToolCallStart {
            id: state.call_id.clone(),
            name: state.name.clone(),
        }];
        if !state.arguments.is_empty() {
            deltas.push(StreamDelta::ToolCallDelta {
                id: state.call_id.clone(),
                partial_json: state.arguments.clone(),
            });
        }
        deltas
    }

    fn append_tool_args(&mut self, item_id: &str, delta: &str) -> Vec<StreamDelta> {
        if delta.is_empty() {
            return Vec::new();
        }
        let state = self.tool_calls.entry(item_id.to_string()).or_default();
        state.arguments.push_str(delta);
        if state.started {
            vec![StreamDelta::ToolCallDelta {
                id: state.call_id.clone(),
                partial_json: delta.to_string(),
            }]
        } else {
            Vec::new()
        }
    }

    fn reconcile_tool_args(
        &mut self,
        item_id: &str,
        arguments: &str,
    ) -> Result<Vec<StreamDelta>, String> {
        let state = self
            .tool_calls
            .get_mut(item_id)
            .ok_or_else(|| "tool call state missing".to_string())?;

        if arguments.is_empty() || arguments == state.arguments {
            return Ok(Vec::new());
        }

        if let Some(suffix) = arguments.strip_prefix(&state.arguments) {
            state.arguments.push_str(suffix);
            if state.started && !suffix.is_empty() {
                return Ok(vec![StreamDelta::ToolCallDelta {
                    id: state.call_id.clone(),
                    partial_json: suffix.to_string(),
                }]);
            }
            return Ok(Vec::new());
        }

        if state.arguments.is_empty() {
            state.arguments = arguments.to_string();
            if state.started {
                return Ok(vec![StreamDelta::ToolCallDelta {
                    id: state.call_id.clone(),
                    partial_json: arguments.to_string(),
                }]);
            }
        }

        Ok(Vec::new())
    }

    fn drain_terminal(&mut self) -> Result<Vec<StreamDelta>, String> {
        if self.terminal_emitted {
            return Ok(Vec::new());
        }

        let Some((input_tokens, output_tokens)) = self.pending_usage else {
            return Err("OpenAI Codex stream ended without terminal response".to_string());
        };

        self.terminal_emitted = true;
        Ok(vec![
            StreamDelta::Usage {
                input_tokens,
                output_tokens,
            },
            StreamDelta::Done {
                stop_reason: Some(self.stop_reason()),
            },
        ])
    }

    fn stop_reason(&self) -> llm::StopReason {
        if self.saw_tool_call {
            return llm::StopReason::ToolUse;
        }

        let Some(reason) = self.pending_incomplete_reason.as_ref() else {
            return llm::StopReason::EndTurn;
        };

        let lower = reason.to_ascii_lowercase();
        if lower.contains("max_output") || lower.contains("length") {
            llm::StopReason::MaxTokens
        } else if lower.contains("tool") {
            llm::StopReason::ToolUse
        } else {
            llm::StopReason::EndTurn
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum CodexResponseEvent {
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        #[serde(default)]
        delta: String,
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        content_index: u32,
    },
    #[serde(rename = "response.output_text.done")]
    OutputTextDone {
        #[serde(default)]
        text: String,
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        content_index: u32,
    },
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded { item: CodexOutputItem },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone { item: CodexOutputItem },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta {
        item_id: String,
        #[serde(default)]
        delta: String,
    },
    #[serde(rename = "response.reasoning_text.delta")]
    ReasoningTextDelta {
        #[serde(default)]
        delta: String,
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        content_index: u32,
    },
    #[serde(rename = "response.reasoning_text.done")]
    ReasoningTextDone {
        #[serde(default)]
        text: String,
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        content_index: u32,
    },
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryTextDelta {
        #[serde(default)]
        delta: String,
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        summary_index: u32,
    },
    #[serde(rename = "response.reasoning_summary_text.done")]
    ReasoningSummaryTextDone {
        #[serde(default)]
        text: String,
        #[serde(default)]
        item_id: String,
        #[serde(default)]
        summary_index: u32,
    },
    #[serde(rename = "response.completed")]
    ResponseCompleted { response: CodexDonePayload },
    #[serde(rename = "response.done")]
    ResponseDone { response: CodexDonePayload },
    #[serde(rename = "response.incomplete")]
    ResponseIncomplete { response: CodexDonePayload },
    #[serde(rename = "response.failed")]
    ResponseFailed { response: CodexFailedPayload },
    #[serde(rename = "error")]
    Error { error: CodexErrorPayload },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct CodexOutputItem {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexDonePayload {
    #[serde(default)]
    incomplete_details: Option<CodexIncompleteDetails>,
    #[serde(default)]
    usage: Option<CodexUsage>,
}

impl CodexDonePayload {
    fn incomplete_reason(&self) -> Option<String> {
        self.incomplete_details
            .as_ref()
            .and_then(|details| details.reason.clone())
    }
}

#[derive(Debug, Deserialize)]
struct CodexIncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct CodexFailedPayload {
    #[serde(default)]
    error: Option<CodexErrorPayload>,
}

impl CodexFailedPayload {
    fn error_message(&self) -> String {
        self.error
            .as_ref()
            .map(|error| error.message.clone())
            .unwrap_or_else(|| "OpenAI Codex request failed".to_string())
    }
}

#[derive(Debug, Deserialize)]
struct CodexErrorPayload {
    #[serde(default)]
    message: String,
}

fn resolve_codex_access_token() -> Option<String> {
    std::env::var(OPENAI_CODEX_ACCESS_TOKEN_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(read_cached_codex_access_token)
}

fn read_cached_codex_access_token() -> Option<String> {
    let home = dirs::home_dir()?;
    let candidates = [
        home.join(".codex").join("auth.json"),
        home.join(".config").join("codex").join("auth.json"),
    ];

    for path in candidates {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(value) = serde_json::from_str::<Value>(&content) {
                if let Some(token) = codex_access_token_from_value(&value) {
                    return Some(token);
                }
            }
        }
    }

    None
}

fn codex_access_token_from_value(value: &Value) -> Option<String> {
    let candidates = [
        value
            .get("tokens")
            .and_then(|tokens| tokens.get("access_token"))
            .and_then(Value::as_str),
        value
            .get("tokens")
            .and_then(|tokens| tokens.get("accessToken"))
            .and_then(Value::as_str),
        value.get("access_token").and_then(Value::as_str),
        value.get("accessToken").and_then(Value::as_str),
        value.get("token").and_then(Value::as_str),
    ];

    candidates
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|token| !token.is_empty() && !token.starts_with("sk-"))
        .map(ToString::to_string)
}

fn extract_chatgpt_account_id(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _signature = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let payload_json: Value = serde_json::from_slice(&decoded).ok()?;
    payload_json
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_test_jwt(account_id: &str) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"alg":"none","typ":"JWT"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::json!({
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": account_id
                }
            })
            .to_string(),
        );
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn extracts_chatgpt_account_id_from_token() {
        let token = build_test_jwt("acct_123");
        assert_eq!(
            extract_chatgpt_account_id(&token).as_deref(),
            Some("acct_123")
        );
    }

    #[test]
    fn extracts_codex_access_token_from_cached_auth() {
        let value = serde_json::json!({
            "tokens": {
                "access_token": "eyJhbGciOiJub25lIn0.payload.sig"
            }
        });

        assert_eq!(
            codex_access_token_from_value(&value).as_deref(),
            Some("eyJhbGciOiJub25lIn0.payload.sig")
        );
    }

    #[test]
    fn ignores_api_keys_in_cached_auth() {
        let value = serde_json::json!({
            "tokens": {
                "access_token": "sk-test-key"
            }
        });

        assert!(codex_access_token_from_value(&value).is_none());
    }

    #[test]
    fn terminal_done_waits_for_late_tool_calls() {
        let mut synth = CodexDeltaSynthesizer::new();

        assert!(synth
            .process_event(r#"{"type":"response.function_call_arguments.delta","item_id":"fc_5","delta":"{\"text\":\"late tool\"}"}"#)
            .unwrap()
            .is_empty());
        assert!(synth
            .process_event(r#"{"type":"response.completed","response":{"incomplete_details":null,"usage":{"input_tokens":1,"output_tokens":1}}}"#)
            .unwrap()
            .is_empty());

        let tool_start = synth
            .process_event(r#"{"type":"response.output_item.done","item":{"type":"function_call","id":"fc_5","call_id":"call_5","name":"echo","arguments":""}}"#)
            .unwrap();
        assert!(tool_start.iter().any(|delta| matches!(
            delta,
            StreamDelta::ToolCallStart { id, name } if id == "call_5" && name == "echo"
        )));

        let terminal = synth.process_event("[DONE]").unwrap();
        assert!(terminal.iter().any(|delta| matches!(
            delta,
            StreamDelta::Done {
                stop_reason: Some(llm::StopReason::ToolUse)
            }
        )));
    }
}
