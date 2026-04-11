use std::net::SocketAddr;
use std::time::Duration;

use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

// ---------------------------------------------------------------------------
// Request / Response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ExecuteRequest {
    tool: String,
    input: serde_json::Value,
}

#[derive(Serialize)]
struct ExecuteResponse {
    output: String,
    is_error: bool,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn health() -> &'static str {
    "ok"
}

async fn execute(Json(req): Json<ExecuteRequest>) -> Json<ExecuteResponse> {
    let result = match req.tool.as_str() {
        "Bash" => execute_bash(&req.input).await,
        "Read" => execute_read(&req.input).await,
        "Write" => execute_write(&req.input).await,
        other => Err(format!("Unknown tool: {other}")),
    };

    match result {
        Ok(output) => Json(ExecuteResponse {
            output,
            is_error: false,
        }),
        Err(msg) => Json(ExecuteResponse {
            output: msg,
            is_error: true,
        }),
    }
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

async fn execute_bash(input: &serde_json::Value) -> Result<String, String> {
    let command = input
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'command' field")?;

    let timeout_ms = input
        .get("timeout")
        .and_then(|v| v.as_u64())
        .unwrap_or(120_000);

    let output = tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        Command::new("/bin/bash").arg("-c").arg(command).output(),
    )
    .await
    .map_err(|_| format!("Command timed out after {timeout_ms}ms"))?
    .map_err(|e| format!("Failed to execute command: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() {
        if stderr.is_empty() {
            Ok(stdout.into_owned())
        } else {
            Ok(format!("{stdout}\n--- stderr ---\n{stderr}"))
        }
    } else {
        let code = output.status.code().unwrap_or(-1);
        Err(format!("Exit code {code}\n{stdout}{stderr}"))
    }
}

async fn execute_read(input: &serde_json::Value) -> Result<String, String> {
    let file_path = input
        .get("file_path")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'file_path' field")?;

    let content = tokio::fs::read_to_string(file_path)
        .await
        .map_err(|e| format!("Failed to read '{file_path}': {e}"))?;

    let offset = input.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let limit = input.get("limit").and_then(|v| v.as_u64()).unwrap_or(2000) as usize;

    let lines: Vec<&str> = content.lines().collect();
    let start = offset.min(lines.len());
    let end = (offset + limit).min(lines.len());

    let numbered: Vec<String> = lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>6}\t{line}", start + i + 1))
        .collect();

    Ok(numbered.join("\n"))
}

async fn execute_write(input: &serde_json::Value) -> Result<String, String> {
    let file_path = input
        .get("file_path")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'file_path' field")?;
    let content = input
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'content' field")?;

    if let Some(parent) = std::path::Path::new(file_path).parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create directories: {e}"))?;
    }

    tokio::fs::write(file_path, content)
        .await
        .map_err(|e| format!("Failed to write '{file_path}': {e}"))?;

    Ok(format!(
        "Successfully wrote {} bytes to {file_path}",
        content.len()
    ))
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let app = Router::new()
        .route("/health", get(health))
        .route("/execute", post(execute));

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    tracing::info!(%addr, "sandbox-tool-server starting");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind");
    axum::serve(listener, app).await.expect("server error");
}
