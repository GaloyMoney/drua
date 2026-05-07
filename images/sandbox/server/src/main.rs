mod session;
mod tracing_middleware;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use sandbox::tool_protocol::{
    BashCommandInput, BashCommandOutput, BashInputError, DeleteInput, GlobInput, GrepInput,
    GrepOutputMode, MoveInput, TextEditorAction, TextEditorInput,
};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::instrument;

use session::{BashSession, SharedSession};

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

#[derive(Deserialize)]
struct InitializeRequest {
    mode: String,
    #[serde(default)]
    repo_url: Option<String>,
    #[serde(default)]
    branch: Option<String>,
}

/// Empty per memo `019dfebc` M4 — no credentials cross the trust
/// boundary. Git auth flows through the drua git-proxy via the
/// projected SA token, not via files written into the workspace.
#[derive(Deserialize, Default)]
struct AttachRequest {}

#[derive(Debug, Serialize)]
struct ResetRepoResponse {
    /// `true` when the workspace is in the expected post-reset state:
    /// either every reset step succeeded (script printed
    /// `__reset_done__`) or the cwd had no `.git` so reset was a no-op
    /// (script printed `__reset_skipped_no_git__`). `false` means at
    /// least one reset step failed and the workspace state is
    /// unspecified — caller should treat as a soft warning.
    ok: bool,
    /// Concatenated stdout/stderr of the reset script, for observability.
    output: String,
}

#[derive(Debug, Serialize)]
struct InitializeResponse {
    cwd: String,
    exported_system_prompt: Option<ExportedFile>,
    exported_skills: Vec<ExportedSkill>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct ExportedFile {
    file_name: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct ExportedSkill {
    name: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

async fn health() -> &'static str {
    "ok"
}

#[instrument(name = "sandbox.server.execute", skip_all, fields(tool = %req.tool))]
async fn execute(
    State(session): State<SharedSession>,
    Json(req): Json<ExecuteRequest>,
) -> Json<ExecuteResponse> {
    let ExecuteRequest { tool, input } = req;
    // Each handler typed-deserializes its slice of `input`, so a typo
    // (`timeoutMs`, `Pattern`, …) becomes a clean parse error instead
    // of a silently-ignored field.
    let result = match tool.as_str() {
        "bash" => execute_bash(&session, input).await,
        "str_replace_based_edit_tool" => execute_text_editor(&session, input).await,
        "Grep" => execute_grep(&session, input).await,
        "Glob" => execute_glob(&session, input).await,
        "Move" => execute_move(&session, input).await,
        "Delete" => execute_delete(&session, input).await,
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

// Bash tool (Anthropic bash_20250124): commands run through a long-lived
// shell session so background processes survive between calls.

/// Wall-clock timeout for the simple grep/glob handlers. Bash has its
/// own default on [`BashCommandInput::DEFAULT_TIMEOUT_MS`].
const GREP_GLOB_TIMEOUT_MS: u64 = 120_000;

async fn execute_bash(session: &SharedSession, input: serde_json::Value) -> Result<String, String> {
    let input: BashCommandInput =
        serde_json::from_value(input).map_err(|e| format!("Invalid bash input: {e}"))?;

    if input.restart {
        session.restart().await?;
        return Ok(serde_json::to_string(&BashCommandOutput {
            stdout: "Bash session restarted.".to_string(),
            stderr: String::new(),
            exit_code: 0,
            duration_ms: 0,
        })
        .expect("BashCommandOutput is serialisable"));
    }

    let timeout_ms = input.validated_timeout_ms().map_err(|e| e.to_string())?;
    let command = input
        .command
        .ok_or_else(|| BashInputError::MissingCommand.to_string())?;
    let result = session.execute(&command, timeout_ms).await?;

    if result.shell_died {
        return Err(format!(
            "Shell exited before completing the command (exit_code={}, stdout_bytes={}, stderr_bytes={})\n--- stdout ---\n{}\n--- stderr ---\n{}",
            result.exit_code,
            result.stdout.len(),
            result.stderr.len(),
            result.stdout,
            result.stderr,
        ));
    }

    let output = BashCommandOutput {
        stdout: result.stdout,
        stderr: result.stderr,
        exit_code: result.exit_code,
        duration_ms: result.duration_ms,
    };
    Ok(serde_json::to_string(&output).expect("BashCommandOutput is serialisable"))
}

/// Async wrapper that resolves `path` against the session's current
/// scope root and rejects anything that escapes it. The scope is the
/// session's cwd: for `scratch` mode it is the workspace root, for
/// `repo` it is the cloned repo dir.
async fn validate_path(session: &SharedSession, path: &str) -> Result<PathBuf, String> {
    let cwd = session.current_cwd().await;
    let scope = if cwd.is_empty() {
        PathBuf::from(workspace_root())
    } else {
        PathBuf::from(cwd)
    };
    validate_path_against(&scope, path)
}

/// Pure-fn core of `validate_path` — used by both the async wrapper
/// and the unit tests so the latter don't need a real `SharedSession`.
fn validate_path_against(scope: &std::path::Path, path: &str) -> Result<PathBuf, String> {
    let scope_canonical = std::fs::canonicalize(scope)
        .map_err(|e| format!("Cannot resolve scope root '{}': {e}", scope.display()))?;

    let abs_path = if PathBuf::from(path).is_relative() {
        scope.join(path)
    } else {
        PathBuf::from(path)
    };

    let canonical = match std::fs::canonicalize(&abs_path) {
        Ok(p) => p,
        Err(_) => {
            let parent = abs_path
                .parent()
                .ok_or_else(|| format!("Invalid path: no parent directory for '{path}'"))?;
            let parent_canonical = std::fs::canonicalize(parent)
                .map_err(|e| format!("Cannot resolve parent directory: {e}"))?;
            parent_canonical.join(
                abs_path
                    .file_name()
                    .ok_or_else(|| format!("Invalid path: no filename in '{path}'"))?,
            )
        }
    };

    if !canonical.starts_with(&scope_canonical) {
        return Err(format!(
            "Access denied: path '{}' is outside scope '{}'",
            path,
            scope.display()
        ));
    }
    Ok(canonical)
}

async fn execute_text_editor(
    session: &SharedSession,
    input: serde_json::Value,
) -> Result<String, String> {
    let input: TextEditorInput =
        serde_json::from_value(input).map_err(|e| format!("Invalid text-editor input: {e}"))?;
    let action = input
        .resolve()
        .map_err(|e| format!("Invalid text-editor input: {e}"))?;

    match action {
        TextEditorAction::View { path, view_range } => {
            editor_view(session, &path, view_range).await
        }
        TextEditorAction::Create { path, file_text } => {
            editor_create(session, &path, &file_text).await
        }
        TextEditorAction::StrReplace {
            path,
            old_str,
            new_str,
        } => editor_str_replace(session, &path, &old_str, &new_str).await,
        TextEditorAction::Insert {
            path,
            insert_line,
            new_str,
        } => editor_insert(session, &path, insert_line, &new_str).await,
    }
}

async fn editor_view(
    session: &SharedSession,
    raw_path: &str,
    view_range: Option<[i64; 2]>,
) -> Result<String, String> {
    let validated = validate_path(session, raw_path).await?;
    let path = validated.to_str().ok_or("Path is not valid UTF-8")?;

    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("Error: {e}"))?;

    if meta.is_dir() {
        let mut entries = Vec::new();
        let mut dir = tokio::fs::read_dir(path)
            .await
            .map_err(|e| format!("Error reading directory: {e}"))?;
        while let Some(entry) = dir
            .next_entry()
            .await
            .map_err(|e| format!("Error reading entry: {e}"))?
        {
            let name = entry.file_name().to_string_lossy().to_string();
            let ft = entry.file_type().await.ok();
            let suffix = if ft.as_ref().is_some_and(|t| t.is_dir()) {
                "/"
            } else {
                ""
            };
            entries.push(format!("{name}{suffix}"));
        }
        entries.sort();
        Ok(entries.join("\n"))
    } else {
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| format!("Error: {e}"))?;
        let lines: Vec<&str> = content.lines().collect();
        let (start, end) = if let Some([s, e]) = view_range {
            let s = (s.max(1) as usize).min(lines.len().max(1));
            let end = if e == -1 {
                lines.len()
            } else {
                (e as usize).min(lines.len())
            };
            (s, end)
        } else {
            (1, lines.len())
        };
        // Line numbering is the Read tool's responsibility, not the protocol.
        Ok(lines[start.saturating_sub(1)..end].join("\n"))
    }
}

async fn editor_create(
    session: &SharedSession,
    raw_path: &str,
    file_text: &str,
) -> Result<String, String> {
    // Create parent dirs first so validate_path can canonicalize the parent.
    if let Some(parent) = std::path::Path::new(raw_path).parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Error creating directories: {e}"))?;
    }
    let validated = validate_path(session, raw_path).await?;
    let path = validated.to_str().ok_or("Path is not valid UTF-8")?;

    tokio::fs::write(path, file_text)
        .await
        .map_err(|e| format!("Error: {e}"))?;

    Ok(format!("File created successfully at: {path}"))
}

async fn editor_str_replace(
    session: &SharedSession,
    raw_path: &str,
    old_str: &str,
    new_str: &str,
) -> Result<String, String> {
    let validated = validate_path(session, raw_path).await?;
    let path = validated.to_str().ok_or("Path is not valid UTF-8")?;

    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| format!("Error: {e}"))?;

    let count = content.matches(old_str).count();
    match count {
        0 => Err(
            "Error: No match found for replacement. Please check your text and try again."
                .to_string(),
        ),
        1 => {
            let new_content = content.replacen(old_str, new_str, 1);
            tokio::fs::write(path, &new_content)
                .await
                .map_err(|e| format!("Error: {e}"))?;
            Ok("Successfully replaced text at exactly one location.".to_string())
        }
        n => Err(format!(
            "Error: Found {n} matches for replacement text. \
             Please provide more context to make a unique match."
        )),
    }
}

/// `insert_line == 0` means insert at the beginning of the file.
async fn editor_insert(
    session: &SharedSession,
    raw_path: &str,
    insert_line: u64,
    insert_text: &str,
) -> Result<String, String> {
    let validated = validate_path(session, raw_path).await?;
    let path = validated.to_str().ok_or("Path is not valid UTF-8")?;
    let insert_line = insert_line as usize;

    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| format!("Error: {e}"))?;

    let mut lines: Vec<&str> = content.lines().collect();
    let insert_at = insert_line.min(lines.len());

    let new_lines: Vec<&str> = insert_text.lines().collect();
    for (i, line) in new_lines.iter().enumerate() {
        lines.insert(insert_at + i, line);
    }

    let new_content = lines.join("\n");
    let new_content = if content.ends_with('\n') {
        format!("{new_content}\n")
    } else {
        new_content
    };

    tokio::fs::write(path, &new_content)
        .await
        .map_err(|e| format!("Error: {e}"))?;

    Ok(format!(
        "Successfully inserted {} lines after line {insert_line}.",
        new_lines.len()
    ))
}

async fn execute_move(session: &SharedSession, input: serde_json::Value) -> Result<String, String> {
    let MoveInput {
        from: from_raw,
        to: to_raw,
    } = serde_json::from_value(input).map_err(|e| format!("Invalid move input: {e}"))?;

    if let Some(parent) = std::path::Path::new(&to_raw).parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Error creating directories: {e}"))?;
    }
    let from = validate_path(session, &from_raw).await?;
    let to = validate_path(session, &to_raw).await?;

    if !tokio::fs::try_exists(&from)
        .await
        .map_err(|e| format!("Error: {e}"))?
    {
        return Err(format!("Error: source does not exist: {from_raw}"));
    }
    if tokio::fs::try_exists(&to)
        .await
        .map_err(|e| format!("Error: {e}"))?
    {
        return Err(format!("Error: destination already exists: {to_raw}"));
    }

    tokio::fs::rename(&from, &to)
        .await
        .map_err(|e| format!("Error: {e}"))?;

    Ok(format!("Moved {from_raw} -> {to_raw}"))
}

async fn execute_delete(
    session: &SharedSession,
    input: serde_json::Value,
) -> Result<String, String> {
    let DeleteInput { path: raw_path } =
        serde_json::from_value(input).map_err(|e| format!("Invalid delete input: {e}"))?;
    let validated = validate_path(session, &raw_path).await?;

    let exists = tokio::fs::try_exists(&validated)
        .await
        .map_err(|e| format!("Error: {e}"))?;
    if !exists {
        return Ok(format!("File already absent: {raw_path}"));
    }
    tokio::fs::remove_file(&validated)
        .await
        .map_err(|e| format!("Error: {e}"))?;
    Ok(format!("Deleted {raw_path}"))
}

async fn execute_grep(session: &SharedSession, input: serde_json::Value) -> Result<String, String> {
    let input: GrepInput =
        serde_json::from_value(input).map_err(|e| format!("Invalid grep input: {e}"))?;

    let mut args: Vec<String> = vec!["--no-heading".to_string(), "--color=never".to_string()];

    let output_mode = input.output_mode.unwrap_or_default();
    match output_mode {
        GrepOutputMode::FilesWithMatches => args.push("--files-with-matches".to_string()),
        GrepOutputMode::Count => args.push("--count".to_string()),
        GrepOutputMode::Content => {}
    }

    if input.case_insensitive {
        args.push("--ignore-case".to_string());
    }

    if matches!(output_mode, GrepOutputMode::Content) {
        // `-n` defaults to true when content is being printed.
        if input.line_numbers.unwrap_or(true) {
            args.push("--line-number".to_string());
        }
    }

    if let Some(n) = input.after_context {
        args.push(format!("--after-context={n}"));
    }
    if let Some(n) = input.before_context {
        args.push(format!("--before-context={n}"));
    }
    if let Some(n) = input.context {
        args.push(format!("--context={n}"));
    }

    if input.multiline {
        args.push("--multiline".to_string());
        args.push("--multiline-dotall".to_string());
    }

    if let Some(ty) = &input.type_ {
        args.push(format!("--type={ty}"));
    }

    if let Some(glob) = &input.glob {
        args.push(format!("--glob={glob}"));
    }

    args.push("--".to_string());
    args.push(input.pattern);

    let search_path = input.path.unwrap_or_else(|| ".".to_string());
    args.push(search_path);

    let scope = session.current_cwd().await;
    let output = tokio::time::timeout(
        Duration::from_millis(GREP_GLOB_TIMEOUT_MS),
        Command::new("rg").args(&args).current_dir(&scope).output(),
    )
    .await
    .map_err(|_| format!("Grep timed out after {GREP_GLOB_TIMEOUT_MS}ms"))?
    .map_err(|e| format!("Failed to execute rg: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // rg exit codes: 0 = matches found, 1 = no matches, 2 = error
    match output.status.code() {
        Some(0) | Some(1) => {
            let mut result = stdout.into_owned();

            if let Some(limit) = input.head_limit {
                let limit = limit as usize;
                let lines: Vec<&str> = result.lines().take(limit).collect();
                result = lines.join("\n");
            }

            Ok(result)
        }
        _ => Err(format!("rg failed: {stderr}")),
    }
}

async fn execute_glob(session: &SharedSession, input: serde_json::Value) -> Result<String, String> {
    let input: GlobInput =
        serde_json::from_value(input).map_err(|e| format!("Invalid glob input: {e}"))?;

    let search_path = input.path.unwrap_or_else(|| ".".to_string());

    let scope = session.current_cwd().await;
    let output = tokio::time::timeout(
        Duration::from_millis(GREP_GLOB_TIMEOUT_MS),
        Command::new("rg")
            .args([
                "--files",
                "--color=never",
                &format!("--glob={pattern}", pattern = input.pattern),
                &search_path,
            ])
            .current_dir(&scope)
            .output(),
    )
    .await
    .map_err(|_| format!("Glob timed out after {GREP_GLOB_TIMEOUT_MS}ms"))?
    .map_err(|e| format!("Failed to execute rg: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    match output.status.code() {
        Some(0) | Some(1) => Ok(stdout.into_owned()),
        _ => Err(format!("rg --files failed: {stderr}")),
    }
}

const DEFAULT_WORKSPACE_ROOT: &str = "/workspace";

fn workspace_root() -> String {
    std::env::var("WORKSPACE_ROOT").unwrap_or_else(|_| DEFAULT_WORKSPACE_ROOT.to_string())
}

#[instrument(name = "sandbox.server.initialize", skip_all, fields(mode = %req.mode))]
async fn initialize(
    State(session): State<SharedSession>,
    Json(req): Json<InitializeRequest>,
) -> Json<InitializeResponse> {
    let resp = initialize_inner(
        &workspace_root(),
        &req.mode,
        req.repo_url.as_deref(),
        req.branch.as_deref(),
    )
    .await;

    // Pin the bash session's cwd to whatever the mode-specific
    // initializer chose. Empty cwd or an error response leaves the
    // existing cwd alone.
    if resp.0.error.is_none() {
        session.set_cwd(resp.0.cwd.clone()).await;
    }

    resp
}

/// Per-agent attach hook. Called by core whenever a new agent
/// attaches to this sandbox so the new tenant gets a clean shell
/// pinned to the cwd `/initialize` recorded — i.e. doesn't inherit
/// the previous tenant's `cd`. Re-pins the bash session's cwd to
/// drop any active shell, then runs a smoke test through a fresh
/// shell so a broken bash environment fails the attach immediately
/// instead of letting the agent flail through dozens of `Exit code
/// 1` results.
///
/// Empty body per memo `019dfebc` M4 — no credentials are written;
/// git auth flows through the drua git-proxy via the projected SA token.
#[instrument(name = "sandbox.server.attach", skip_all)]
async fn attach(
    State(session): State<SharedSession>,
    _body: Option<Json<AttachRequest>>,
) -> Result<&'static str, (StatusCode, String)> {
    let cwd = session.current_cwd().await;
    session.set_cwd(cwd).await;

    const SMOKE_TIMEOUT_MS: u64 = 10_000;
    const SMOKE_MARKER: &str = "__sandbox_attach_ready__";
    let smoke_cmd = format!("echo {SMOKE_MARKER}");
    match session.execute(&smoke_cmd, SMOKE_TIMEOUT_MS).await {
        Ok(r) if !r.shell_died && r.exit_code == 0 && r.stdout.contains(SMOKE_MARKER) => Ok("ok"),
        Ok(r) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "attach smoke test failed: shell_died={}, exit_code={}, stdout={:?}, stderr={:?}",
                r.shell_died, r.exit_code, r.stdout, r.stderr
            ),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("attach smoke test failed: {e}"),
        )),
    }
}

/// Resets the cloned repo at the bash session's cwd to a clean baseline:
/// `git fetch origin main && git checkout main && git reset --hard origin/main
/// && git clean -fd`, then drops any leftover `bot/*` branches.
///
/// Runs through the existing bash session so the commands inherit the
/// UID-drop and the agent's git config. The script body runs in a
/// **subshell** with `set -e` so a failing step (e.g. `git fetch` on a
/// stale token) aborts the chain without printing the success marker —
/// AND without `exit` killing the persistent shell.
///
/// Markers (mutually exclusive — at most one prints to stdout):
/// - `__reset_done__` — every reset step succeeded.
/// - `__reset_skipped_no_git__` — cwd has no `.git`; nothing to reset
///   (e.g. scratch-mode sandboxes).
/// - neither — at least one reset step failed; workspace state is
///   unspecified.
///
/// `ok = output.contains(__reset_done__) || output.contains(__reset_skipped_no_git__)`.
#[instrument(name = "sandbox.server.reset_repo", skip_all)]
async fn reset_repo(State(session): State<SharedSession>) -> Json<ResetRepoResponse> {
    let cwd = session.current_cwd().await;
    // Subshell `( ... )` confines `exit` so the no-`.git` short-circuit
    // and any `set -e` abort don't terminate the persistent bash
    // session. Outer `|| true` keeps the session's last exit code 0
    // (the script itself is success-via-marker, not via exit code).
    let script = format!(
        r#"( set -e; cd {cwd}; [ -d .git ] || {{ echo __reset_skipped_no_git__; exit 0; }}; \
git fetch origin main; \
git checkout main 2>/dev/null || git checkout -B main origin/main; \
git reset --hard origin/main; \
git clean -fd; \
for b in $(git branch --list 'bot/*' | sed 's/^[* ]*//'); do git branch -D "$b" || true; done; \
echo __reset_done__ ) || true"#
    );

    const RESET_TIMEOUT_MS: u64 = 60_000;
    match session.execute(&script, RESET_TIMEOUT_MS).await {
        Ok(r) => Json(ResetRepoResponse {
            ok: !r.shell_died
                && (r.stdout.contains("__reset_done__")
                    || r.stdout.contains("__reset_skipped_no_git__")),
            output: r.stdout,
        }),
        Err(e) => Json(ResetRepoResponse {
            ok: false,
            output: e,
        }),
    }
}

async fn initialize_inner(
    workspace: &str,
    mode: &str,
    repo_url: Option<&str>,
    branch: Option<&str>,
) -> Json<InitializeResponse> {
    let result = match mode {
        "scratch" => initialize_scratch(workspace).await,
        "repo" => initialize_repo(workspace, repo_url, branch).await,
        other => Err(format!(
            "Unknown mode: {other}. Expected 'scratch' or 'repo'."
        )),
    };

    match result {
        Ok(resp) => Json(resp),
        Err(msg) => Json(InitializeResponse {
            cwd: workspace.to_string(),
            exported_system_prompt: None,
            exported_skills: Vec::new(),
            error: Some(msg),
        }),
    }
}

async fn initialize_scratch(workspace: &str) -> Result<InitializeResponse, String> {
    tokio::fs::create_dir_all(workspace)
        .await
        .map_err(|e| format!("Failed to create workspace: {e}"))?;

    Ok(InitializeResponse {
        cwd: workspace.to_string(),
        exported_system_prompt: None,
        exported_skills: Vec::new(),
        error: None,
    })
}

async fn initialize_repo(
    workspace: &str,
    repo_url: Option<&str>,
    branch: Option<&str>,
) -> Result<InitializeResponse, String> {
    let repo_url = repo_url
        .filter(|u| !u.is_empty())
        .ok_or("Missing 'repo_url' for repo mode")?;

    let repo_name = extract_repo_name(repo_url)?;
    let repos_dir = PathBuf::from(workspace).join("repos");
    let clone_dir = repos_dir.join(&repo_name);

    tokio::fs::create_dir_all(&repos_dir)
        .await
        .map_err(|e| format!("Failed to create repos directory: {e}"))?;

    // Idempotent across sandbox restarts: only treat as already cloned when
    // `.git` exists; a stray empty dir still triggers a fresh clone.
    let already_cloned = clone_dir.join(".git").is_dir();
    if !already_cloned {
        let mut args = vec!["clone"];
        if let Some(b) = branch.filter(|b| !b.is_empty()) {
            args.extend(["--branch", b]);
        }
        args.extend([repo_url, clone_dir.to_str().unwrap()]);

        let output = Command::new("git")
            .args(&args)
            .output()
            .await
            .map_err(|e| format!("Failed to run git clone: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git clone failed: {stderr}"));
        }
    }

    // The clone (and the surrounding `create_dir_all`) runs as the
    // sandbox-tool-server's uid — root in the prod container. The agent's
    // `bash` tool drops to uid 1000 via the UidOnly isolation layer, so
    // any subsequent git command inside the repo would trip the
    // safe.directory check ("fatal: detected dubious ownership"). chown
    // the tree to the agent uid up front so the agent doesn't have to
    // burn a turn applying `git config --add safe.directory` every run.
    //
    // Best-effort: in dev / CI the test process isn't root and lacks
    // CAP_CHOWN, but it also already owns the files it just cloned, so
    // git is happy without this step.
    chown_to_agent(&clone_dir).await;

    let system_prompt = scan_claude_md(&clone_dir).await;
    let skills = scan_skills(&clone_dir).await;

    Ok(InitializeResponse {
        cwd: clone_dir.to_string_lossy().to_string(),
        exported_system_prompt: system_prompt,
        exported_skills: skills,
        error: None,
    })
}

async fn chown_to_agent(path: &Path) {
    let Some(p) = path.to_str() else {
        tracing::warn!(?path, "skipping chown: path is not valid UTF-8");
        return;
    };
    match Command::new("chown")
        .args(["-R", "1000:1000", p])
        .output()
        .await
    {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            tracing::warn!(
                exit_code = o.status.code().unwrap_or(-1),
                stderr = ?String::from_utf8_lossy(&o.stderr).trim(),
                path = p,
                "chown -R 1000:1000 failed, continuing",
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, path = p, "failed to spawn chown, continuing");
        }
    }
}

fn extract_repo_name(url: &str) -> Result<String, String> {
    let path = url.trim_end_matches('/');
    let name = path
        .rsplit('/')
        .next()
        .ok_or_else(|| format!("Cannot extract repo name from URL: {url}"))?;

    if name.is_empty() {
        return Err(format!("Cannot extract repo name from URL: {url}"));
    }

    let name = name.strip_suffix(".git").unwrap_or(name);
    if name.is_empty() {
        return Err(format!("Cannot extract repo name from URL: {url}"));
    }

    Ok(name.to_string())
}

async fn scan_claude_md(repo_dir: &Path) -> Option<ExportedFile> {
    let path = repo_dir.join("CLAUDE.md");
    tokio::fs::read_to_string(&path)
        .await
        .ok()
        .map(|content| ExportedFile {
            file_name: "CLAUDE.md".to_string(),
            content,
        })
}

/// Scan a cloned repo for skill definitions.
///
/// Supports two layouts:
/// 1. **Anthropic skills** — `.claude/skills/<name>/SKILL.md` (per-skill
///    directory with a canonical `SKILL.md`; used by e.g. lana-bank).
/// 2. **Flat commands** — `.claude/commands/<name>.md` (single-file slash
///    command style).
///
/// Layout 1 takes priority; a skill already picked up from there is not
/// overridden by a same-named file in `commands/`. Results are sorted by
/// name.
async fn scan_skills(repo_dir: &Path) -> Vec<ExportedSkill> {
    let mut skills: Vec<ExportedSkill> = Vec::new();
    let claude_dir = repo_dir.join(".claude");

    let skills_dir = claude_dir.join("skills");
    if let Ok(mut dir) = tokio::fs::read_dir(&skills_dir).await {
        while let Ok(Some(entry)) = dir.next_entry().await {
            let Ok(ft) = entry.file_type().await else {
                continue;
            };
            if !ft.is_dir() {
                continue;
            }
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let skill_file = path.join("SKILL.md");
            if let Ok(raw) = tokio::fs::read_to_string(&skill_file).await {
                let (description, content) = parse_skill_frontmatter(&raw);
                skills.push(ExportedSkill {
                    name: name.to_string(),
                    content,
                    description,
                });
            }
        }
    }

    // Layout 2 (legacy): .claude/commands/*.md, skipped if already in layout 1.
    let commands_dir = claude_dir.join("commands");
    if let Ok(mut dir) = tokio::fs::read_dir(&commands_dir).await {
        while let Ok(Some(entry)) = dir.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if name.is_empty() || skills.iter().any(|s| s.name == name) {
                continue;
            }
            if let Ok(content) = tokio::fs::read_to_string(&path).await {
                skills.push(ExportedSkill {
                    name: name.to_string(),
                    content,
                    description: None,
                });
            }
        }
    }

    skills.sort_by_key(|a| a.name.clone());
    skills
}

#[derive(serde::Deserialize, Default)]
struct SkillFrontmatter {
    #[serde(default)]
    description: Option<String>,
}

/// Returns `(description, body_without_frontmatter)`. If there is no
/// `---`-delimited frontmatter, returns `(None, original_content)`.
fn parse_skill_frontmatter(raw: &str) -> (Option<String>, String) {
    let trimmed = raw.trim_start();
    let Some(rest) = trimmed.strip_prefix("---") else {
        return (None, raw.to_string());
    };

    let Some((frontmatter_str, after_fm)) = rest.split_once("\n---") else {
        return (None, raw.to_string());
    };

    let fm: SkillFrontmatter = serde_yaml::from_str(frontmatter_str.trim()).unwrap_or_default();
    let body = after_fm.trim_start_matches(['-', '\n']).to_string();

    (fm.description.filter(|d| !d.is_empty()), body)
}

/// Builds the axum router with the inbound traceparent middleware
/// applied. Exposed so integration tests can drive it via `tower::ServiceExt`.
pub fn build_app(session: SharedSession) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/execute", post(execute))
        .route("/initialize", post(initialize))
        .route("/attach", post(attach))
        .route("/reset-repo", post(reset_repo))
        .layer(axum::middleware::from_fn(
            tracing_middleware::trace_context_middleware,
        ))
        .with_state(session)
}

#[tokio::main]
async fn main() {
    if let Err(e) = drua_tracing_utils::init_tracer(drua_tracing_utils::TracingConfig {
        service_name: std::env::var("OTEL_SERVICE_NAME")
            .unwrap_or_else(|_| "drua-sandbox".to_string()),
    }) {
        eprintln!("Failed to init tracer: {e}");
    }

    let session: SharedSession = Arc::new(BashSession::new());
    let app = build_app(session);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    tracing::info!(%addr, "sandbox-tool-server starting");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind");
    let serve_result = axum::serve(listener, app).await;

    if let Err(e) = drua_tracing_utils::shutdown_tracer() {
        eprintln!("Error shutting down tracer: {e}");
    }

    serve_result.expect("server error");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Set WORKSPACE_ROOT to the system temp dir so that tests using
    /// `std::env::temp_dir()` pass path validation. Called once per
    /// process.
    fn init_test_workspace() {
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            let tmp = std::env::temp_dir();
            std::fs::create_dir_all(&tmp).unwrap();
            std::env::set_var("WORKSPACE_ROOT", tmp.to_str().unwrap());
        });
    }

    fn test_session() -> SharedSession {
        Arc::new(BashSession::new())
    }

    // Same extraction the inbound middleware uses, without the axum stack.
    #[test]
    fn propagator_extracts_traceparent_trace_id() {
        use opentelemetry::propagation::TextMapPropagator;
        use opentelemetry::trace::TraceContextExt;
        use opentelemetry_http::HeaderExtractor;
        use opentelemetry_sdk::propagation::TraceContextPropagator;

        let propagator = TraceContextPropagator::new();

        let trace_id = "0af7651916cd43dd8448eb211c80319c";
        let span_id = "b7ad6b7169203331";
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "traceparent",
            format!("00-{trace_id}-{span_id}-01").parse().unwrap(),
        );

        let cx = propagator.extract(&HeaderExtractor(&headers));
        let span_ctx = cx.span().span_context().clone();

        assert!(
            span_ctx.is_valid(),
            "extracted span context should be valid"
        );
        assert_eq!(span_ctx.trace_id().to_string(), trace_id);
        assert_eq!(span_ctx.span_id().to_string(), span_id);
    }

    // Smoke: a `traceparent`-bearing request passes the middleware.
    #[tokio::test]
    async fn middleware_accepts_traceparent_header() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use opentelemetry::global;
        use opentelemetry_sdk::propagation::TraceContextPropagator;
        use tower::ServiceExt;

        global::set_text_map_propagator(TraceContextPropagator::new());

        let app = build_app(test_session());
        let req = Request::builder()
            .uri("/health")
            .header(
                "traceparent",
                "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
            )
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn bash_executes_echo() {
        init_test_workspace();
        let session = test_session();
        let input = serde_json::json!({"command": "echo hello"});
        let result = execute_bash(&session, input).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("hello"));
    }

    #[tokio::test]
    async fn bash_nonzero_exit_surfaces_in_structured_output() {
        init_test_workspace();
        let session = test_session();
        let input = serde_json::json!({"command": "bash -c 'exit 42'"});
        let result = execute_bash(&session, input).await.expect("ok envelope");
        let parsed: BashCommandOutput = serde_json::from_str(&result).expect("json");
        assert_eq!(parsed.exit_code, 42);
    }

    #[tokio::test]
    async fn bash_restart_returns_ok() {
        init_test_workspace();
        let session = test_session();
        let input = serde_json::json!({"restart": true});
        let result = execute_bash(&session, input).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("restarted"));
    }

    #[tokio::test]
    async fn bash_missing_command_returns_error() {
        init_test_workspace();
        let session = test_session();
        let input = serde_json::json!({});
        let result = execute_bash(&session, input).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("command"));
    }

    #[tokio::test]
    async fn bash_rejects_string_timeout_ms_at_parse() {
        init_test_workspace();
        let session = test_session();
        let input = serde_json::json!({"command": "echo hi", "timeout_ms": "1000"});
        let err = execute_bash(&session, input)
            .await
            .expect_err("string timeout_ms should fail at parse");
        assert!(
            err.contains("Invalid bash input"),
            "expected parse error, got: {err}"
        );
    }

    #[tokio::test]
    async fn bash_rejects_unknown_field() {
        init_test_workspace();
        let session = test_session();
        let input = serde_json::json!({"command": "echo hi", "timeoutMs": 1000});
        let err = execute_bash(&session, input)
            .await
            .expect_err("unknown field should fail at parse");
        assert!(
            err.contains("Invalid bash input"),
            "expected parse error, got: {err}"
        );
    }

    #[tokio::test]
    async fn bash_rejects_zero_timeout_ms() {
        init_test_workspace();
        let session = test_session();
        let input = serde_json::json!({"command": "echo hi", "timeout_ms": 0});
        let err = execute_bash(&session, input)
            .await
            .expect_err("zero timeout should be rejected");
        assert!(err.contains("greater than 0"), "got: {err}");
    }

    #[tokio::test]
    async fn bash_rejects_too_large_timeout_ms() {
        init_test_workspace();
        let session = test_session();
        let input = serde_json::json!({
            "command": "echo hi",
            "timeout_ms": BashCommandInput::MAX_TIMEOUT_MS + 1,
        });
        let err = execute_bash(&session, input)
            .await
            .expect_err("too-large timeout should be rejected");
        assert!(err.contains("at most"), "got: {err}");
    }

    #[tokio::test]
    async fn bash_honors_custom_timeout_ms() {
        init_test_workspace();
        let session = test_session();
        let input = serde_json::json!({
            "command": "trap 'true' INT; sleep 30",
            "timeout_ms": 50,
        });

        let result = execute_bash(&session, input).await;
        let err = result.expect_err("command should hit custom timeout");
        assert!(
            err.contains("timed out"),
            "expected timeout error, got: {err}"
        );
    }

    /// `exec false` replaces the shell with `false` (exit 1) before the
    /// marker echo can run, so the marker never arrives. The error
    /// must (a) be tagged "Shell exited before completing the command"
    /// rather than the generic "Exit code N" path, (b) carry the bytes
    /// the shell printed to stdout/stderr before dying.
    #[tokio::test]
    async fn bash_surfaces_partial_output_when_shell_dies() {
        init_test_workspace();
        let session = test_session();
        let input = serde_json::json!({
            "command": "echo on-stdout; echo on-stderr >&2; exec false",
        });
        let result = execute_bash(&session, input).await;
        let err = result.expect_err("shell death should surface as Err");
        assert!(
            err.contains("Shell exited before completing the command"),
            "expected shell-died prefix, got: {err}"
        );
        assert!(err.contains("on-stdout"), "stdout missing: {err}");
        assert!(err.contains("on-stderr"), "stderr missing: {err}");
    }

    #[tokio::test]
    async fn attach_smoke_test_succeeds_on_healthy_session() {
        init_test_workspace();
        let session = test_session();
        let result = attach(State(session), None).await;
        assert!(result.is_ok(), "attach failed: {:?}", result.err());
        assert_eq!(result.unwrap(), "ok");
    }

    #[tokio::test]
    async fn editor_create_and_view() {
        init_test_workspace();
        let dir = std::env::temp_dir().join("sandbox-test-create-view");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let file = dir.join("test.txt");

        let create_input = serde_json::json!({
            "command": "create",
            "path": file.to_str().unwrap(),
            "file_text": "line one\nline two\nline three"
        });
        let result = execute_text_editor(&test_session(), create_input).await;
        assert!(result.is_ok(), "create failed: {:?}", result);

        let view_input = serde_json::json!({
            "command": "view",
            "path": file.to_str().unwrap()
        });
        let result = execute_text_editor(&test_session(), view_input)
            .await
            .unwrap();
        assert_eq!(result, "line one\nline two\nline three");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn editor_view_with_range() {
        init_test_workspace();
        let dir = std::env::temp_dir().join("sandbox-test-view-range");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let file = dir.join("range.txt");

        let create_input = serde_json::json!({
            "command": "create",
            "path": file.to_str().unwrap(),
            "file_text": "a\nb\nc\nd\ne"
        });
        execute_text_editor(&test_session(), create_input)
            .await
            .unwrap();

        let view_input = serde_json::json!({
            "command": "view",
            "path": file.to_str().unwrap(),
            "view_range": [2, 4]
        });
        let result = execute_text_editor(&test_session(), view_input)
            .await
            .unwrap();
        assert_eq!(result, "b\nc\nd");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn editor_view_directory() {
        init_test_workspace();
        let dir = std::env::temp_dir().join("sandbox-test-view-dir");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("file_a.txt"), "a").await.unwrap();
        tokio::fs::write(dir.join("file_b.txt"), "b").await.unwrap();

        let view_input = serde_json::json!({
            "command": "view",
            "path": dir.to_str().unwrap()
        });
        let result = execute_text_editor(&test_session(), view_input)
            .await
            .unwrap();
        assert!(result.contains("file_a.txt"));
        assert!(result.contains("file_b.txt"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn editor_str_replace_single_match() {
        init_test_workspace();
        let dir = std::env::temp_dir().join("sandbox-test-replace");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let file = dir.join("replace.txt");

        let create_input = serde_json::json!({
            "command": "create",
            "path": file.to_str().unwrap(),
            "file_text": "hello world\ngoodbye world"
        });
        execute_text_editor(&test_session(), create_input)
            .await
            .unwrap();

        let replace_input = serde_json::json!({
            "command": "str_replace",
            "path": file.to_str().unwrap(),
            "old_str": "hello world",
            "new_str": "hello rust"
        });
        let result = execute_text_editor(&test_session(), replace_input).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("Successfully replaced"));

        let content = tokio::fs::read_to_string(&file).await.unwrap();
        assert!(content.contains("hello rust"));
        assert!(content.contains("goodbye world"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn editor_str_replace_no_match() {
        init_test_workspace();
        let dir = std::env::temp_dir().join("sandbox-test-replace-no-match");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let file = dir.join("no_match.txt");

        let create_input = serde_json::json!({
            "command": "create",
            "path": file.to_str().unwrap(),
            "file_text": "hello world"
        });
        execute_text_editor(&test_session(), create_input)
            .await
            .unwrap();

        let replace_input = serde_json::json!({
            "command": "str_replace",
            "path": file.to_str().unwrap(),
            "old_str": "nonexistent",
            "new_str": "replacement"
        });
        let result = execute_text_editor(&test_session(), replace_input).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No match found"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn editor_str_replace_multiple_matches() {
        init_test_workspace();
        let dir = std::env::temp_dir().join("sandbox-test-replace-multi");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let file = dir.join("multi.txt");

        let create_input = serde_json::json!({
            "command": "create",
            "path": file.to_str().unwrap(),
            "file_text": "foo bar foo"
        });
        execute_text_editor(&test_session(), create_input)
            .await
            .unwrap();

        let replace_input = serde_json::json!({
            "command": "str_replace",
            "path": file.to_str().unwrap(),
            "old_str": "foo",
            "new_str": "baz"
        });
        let result = execute_text_editor(&test_session(), replace_input).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("2 matches"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn editor_insert_at_beginning() {
        init_test_workspace();
        let dir = std::env::temp_dir().join("sandbox-test-insert");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let file = dir.join("insert.txt");

        let create_input = serde_json::json!({
            "command": "create",
            "path": file.to_str().unwrap(),
            "file_text": "line one\nline two\n"
        });
        execute_text_editor(&test_session(), create_input)
            .await
            .unwrap();

        let insert_input = serde_json::json!({
            "command": "insert",
            "path": file.to_str().unwrap(),
            "insert_line": 0,
            "new_str": "header line"
        });
        let result = execute_text_editor(&test_session(), insert_input).await;
        assert!(result.is_ok(), "insert failed: {:?}", result);

        let content = tokio::fs::read_to_string(&file).await.unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines[0], "header line");
        assert_eq!(lines[1], "line one");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn editor_insert_accepts_legacy_insert_text_alias() {
        init_test_workspace();
        let dir = std::env::temp_dir().join("sandbox-test-insert-alias");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let file = dir.join("alias.txt");

        let create_input = serde_json::json!({
            "command": "create",
            "path": file.to_str().unwrap(),
            "file_text": "line one\n"
        });
        execute_text_editor(&test_session(), create_input)
            .await
            .unwrap();

        // Older callers send `insert_text` instead of `new_str`. The
        // typed shape accepts both via #[serde(alias)].
        let insert_input = serde_json::json!({
            "command": "insert",
            "path": file.to_str().unwrap(),
            "insert_line": 1,
            "insert_text": "added"
        });
        let result = execute_text_editor(&test_session(), insert_input).await;
        assert!(result.is_ok(), "alias insert failed: {:?}", result);
    }

    #[tokio::test]
    async fn editor_view_nonexistent_file_returns_error() {
        init_test_workspace();
        let input = serde_json::json!({
            "command": "view",
            "path": "/nonexistent/path/file.txt"
        });
        let result = execute_text_editor(&test_session(), input).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn text_editor_rejects_unknown_command() {
        init_test_workspace();
        let input = serde_json::json!({"command": "delete"});
        let err = execute_text_editor(&test_session(), input)
            .await
            .expect_err("unknown command should fail at parse");
        assert!(err.contains("Invalid text-editor input"), "got: {err}");
    }

    #[tokio::test]
    async fn text_editor_rejects_unknown_field() {
        init_test_workspace();
        let input = serde_json::json!({
            "command": "view",
            "path": "/tmp/x",
            "viewRange": [1, 5]  // camelCase typo for view_range
        });
        let err = execute_text_editor(&test_session(), input)
            .await
            .expect_err("unknown field should fail at parse");
        assert!(err.contains("Invalid text-editor input"), "got: {err}");
    }

    // ── extract_repo_name ──────────────────────────────────────────

    #[test]
    fn extract_repo_name_https_with_git_suffix() {
        let name = extract_repo_name("https://github.com/org/my-repo.git").unwrap();
        assert_eq!(name, "my-repo");
    }

    #[test]
    fn extract_repo_name_https_without_suffix() {
        let name = extract_repo_name("https://github.com/org/my-repo").unwrap();
        assert_eq!(name, "my-repo");
    }

    #[test]
    fn extract_repo_name_trailing_slash() {
        let name = extract_repo_name("https://github.com/org/my-repo/").unwrap();
        assert_eq!(name, "my-repo");
    }

    #[test]
    fn extract_repo_name_bare_name() {
        let name = extract_repo_name("my-repo.git").unwrap();
        assert_eq!(name, "my-repo");
    }

    #[test]
    fn extract_repo_name_empty_url_fails() {
        assert!(extract_repo_name("").is_err());
    }

    // ── scan_claude_md ─────────────────────────────────────────────

    #[tokio::test]
    async fn scan_claude_md_finds_file() {
        let dir = std::env::temp_dir().join("sandbox-test-claude-md");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("CLAUDE.md"), "# Instructions\nDo stuff")
            .await
            .unwrap();

        let result = scan_claude_md(&dir).await;
        assert!(result.is_some());
        let exported = result.unwrap();
        assert_eq!(exported.file_name, "CLAUDE.md");
        assert!(exported.content.contains("Do stuff"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn scan_claude_md_returns_none_when_missing() {
        let dir = std::env::temp_dir().join("sandbox-test-claude-md-missing");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let result = scan_claude_md(&dir).await;
        assert!(result.is_none());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── scan_skills ────────────────────────────────────────────────

    #[tokio::test]
    async fn scan_skills_finds_md_files() {
        let dir = std::env::temp_dir().join("sandbox-test-skills");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let commands_dir = dir.join(".claude").join("commands");
        tokio::fs::create_dir_all(&commands_dir).await.unwrap();
        tokio::fs::write(commands_dir.join("review.md"), "Review the code")
            .await
            .unwrap();
        tokio::fs::write(commands_dir.join("deploy.md"), "Deploy the app")
            .await
            .unwrap();
        // Non-md file should be ignored
        tokio::fs::write(commands_dir.join("notes.txt"), "ignored")
            .await
            .unwrap();

        let skills = scan_skills(&dir).await;
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "deploy");
        assert_eq!(skills[0].content, "Deploy the app");
        assert_eq!(skills[1].name, "review");
        assert_eq!(skills[1].content, "Review the code");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn scan_skills_returns_empty_when_no_commands_dir() {
        let dir = std::env::temp_dir().join("sandbox-test-skills-empty");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let skills = scan_skills(&dir).await;
        assert!(skills.is_empty());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn scan_skills_finds_anthropic_skills_dir_layout() {
        // `.claude/skills/<name>/SKILL.md` — used by lana-bank and the
        // Anthropic-published skill convention.
        let dir = std::env::temp_dir().join("sandbox-test-skills-anthropic");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let skills_dir = dir.join(".claude").join("skills");
        tokio::fs::create_dir_all(skills_dir.join("lana-qa"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(skills_dir.join("lana-review"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(skills_dir.join("no-skill-md"))
            .await
            .unwrap();
        tokio::fs::write(skills_dir.join("lana-qa").join("SKILL.md"), "QA checks")
            .await
            .unwrap();
        tokio::fs::write(skills_dir.join("lana-review").join("SKILL.md"), "Review PR")
            .await
            .unwrap();

        let skills = scan_skills(&dir).await;
        assert_eq!(skills.len(), 2, "only dirs containing SKILL.md count");
        assert_eq!(skills[0].name, "lana-qa");
        assert_eq!(skills[0].content, "QA checks");
        assert_eq!(skills[1].name, "lana-review");
        assert_eq!(skills[1].content, "Review PR");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn scan_skills_merges_both_layouts_with_skills_dir_priority() {
        // If a name appears in both layouts, the `.claude/skills/<name>/SKILL.md`
        // version wins.
        let dir = std::env::temp_dir().join("sandbox-test-skills-merged");
        let _ = tokio::fs::remove_dir_all(&dir).await;

        let skills_dir = dir.join(".claude").join("skills");
        tokio::fs::create_dir_all(skills_dir.join("shared"))
            .await
            .unwrap();
        tokio::fs::write(skills_dir.join("shared").join("SKILL.md"), "from skills")
            .await
            .unwrap();

        let commands_dir = dir.join(".claude").join("commands");
        tokio::fs::create_dir_all(&commands_dir).await.unwrap();
        tokio::fs::write(commands_dir.join("shared.md"), "from commands")
            .await
            .unwrap();
        tokio::fs::write(commands_dir.join("only-flat.md"), "flat only")
            .await
            .unwrap();

        let skills = scan_skills(&dir).await;
        assert_eq!(skills.len(), 2);
        // Alphabetical: only-flat, shared
        assert_eq!(skills[0].name, "only-flat");
        assert_eq!(skills[0].content, "flat only");
        assert_eq!(skills[1].name, "shared");
        assert_eq!(skills[1].content, "from skills", "skills/ layout wins");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn scan_skills_parses_frontmatter_description() {
        let dir = std::env::temp_dir().join("sandbox-test-skills-fm");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let skills_dir = dir.join(".claude").join("skills");
        tokio::fs::create_dir_all(skills_dir.join("deploy"))
            .await
            .unwrap();

        let skill_md =
            "---\ndescription: Deploy the app to production\n---\n\nRun deploy steps here.";
        tokio::fs::write(skills_dir.join("deploy").join("SKILL.md"), skill_md)
            .await
            .unwrap();

        let skills = scan_skills(&dir).await;
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "deploy");
        assert_eq!(skills[0].content, "Run deploy steps here.");
        assert_eq!(
            skills[0].description.as_deref(),
            Some("Deploy the app to production")
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── parse_skill_frontmatter ──────────────────────────────────

    #[test]
    fn parse_frontmatter_extracts_description() {
        let raw = "---\ndescription: Deploy to prod\ntags: [ci]\n---\n\nBody here.";
        let (desc, body) = parse_skill_frontmatter(raw);
        assert_eq!(desc.as_deref(), Some("Deploy to prod"));
        assert_eq!(body, "Body here.");
    }

    #[test]
    fn parse_frontmatter_handles_quoted_description() {
        let raw = "---\ndescription: \"Run code review\"\n---\n\nReview body.";
        let (desc, body) = parse_skill_frontmatter(raw);
        assert_eq!(desc.as_deref(), Some("Run code review"));
        assert_eq!(body, "Review body.");
    }

    #[test]
    fn parse_frontmatter_returns_none_when_absent() {
        let raw = "Just the body, no frontmatter.";
        let (desc, body) = parse_skill_frontmatter(raw);
        assert!(desc.is_none());
        assert_eq!(body, raw);
    }

    #[test]
    fn parse_frontmatter_returns_none_when_no_description() {
        let raw = "---\ntags: [ci]\n---\n\nBody.";
        let (desc, body) = parse_skill_frontmatter(raw);
        assert!(desc.is_none());
        assert_eq!(body, "Body.");
    }

    // ── initialize_scratch ─────────────────────────────────────────

    #[tokio::test]
    async fn initialize_scratch_returns_workspace() {
        init_test_workspace();
        let dir = std::env::temp_dir().join("sandbox-test-init-scratch");
        let _ = tokio::fs::remove_dir_all(&dir).await;

        let workspace = dir.to_str().unwrap();
        let result = initialize_scratch(workspace).await;
        assert!(result.is_ok());
        let resp = result.unwrap();
        assert_eq!(resp.cwd, workspace);
        assert!(resp.exported_system_prompt.is_none());
        assert!(resp.exported_skills.is_empty());
        assert!(resp.error.is_none());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── initialize_repo ────────────────────────────────────────────

    /// Returns true when `git` is on PATH (not available inside nix build sandbox).
    async fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .await
            .is_ok_and(|o| o.status.success())
    }

    #[tokio::test]
    async fn initialize_repo_with_local_bare_repo() {
        if !git_available().await {
            eprintln!("git not available, skipping");
            return;
        }
        let base = std::env::temp_dir().join("sandbox-test-init-repo");
        let _ = tokio::fs::remove_dir_all(&base).await;
        tokio::fs::create_dir_all(&base).await.unwrap();

        // Create a bare repo with CLAUDE.md and a skill
        let bare_dir = base.join("test-repo.git");
        let work_dir = base.join("work");

        // Init bare repo
        let output = Command::new("git")
            .args(["init", "--bare", bare_dir.to_str().unwrap()])
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "git init --bare failed");

        // Clone it to a work dir, add files, push
        let output = Command::new("git")
            .args([
                "clone",
                bare_dir.to_str().unwrap(),
                work_dir.to_str().unwrap(),
            ])
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "git clone failed");

        tokio::fs::write(work_dir.join("CLAUDE.md"), "# Test instructions")
            .await
            .unwrap();
        let cmds_dir = work_dir.join(".claude").join("commands");
        tokio::fs::create_dir_all(&cmds_dir).await.unwrap();
        tokio::fs::write(cmds_dir.join("review.md"), "Review everything")
            .await
            .unwrap();

        // Configure git user for commit
        let _ = Command::new("git")
            .args([
                "-C",
                work_dir.to_str().unwrap(),
                "config",
                "user.email",
                "test@test.com",
            ])
            .output()
            .await;
        let _ = Command::new("git")
            .args([
                "-C",
                work_dir.to_str().unwrap(),
                "config",
                "user.name",
                "Test",
            ])
            .output()
            .await;
        let _ = Command::new("git")
            .args(["-C", work_dir.to_str().unwrap(), "add", "-f", "."])
            .output()
            .await;
        let output = Command::new("git")
            .args(["-C", work_dir.to_str().unwrap(), "commit", "-m", "init"])
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = Command::new("git")
            .args(["-C", work_dir.to_str().unwrap(), "push"])
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "git push failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Use a temp workspace dir for the clone target
        let workspace = base.join("workspace");
        let result = initialize_repo(
            workspace.to_str().unwrap(),
            Some(bare_dir.to_str().unwrap()),
            None,
        )
        .await;
        assert!(result.is_ok(), "initialize_repo failed: {:?}", result);
        let resp = result.unwrap();
        assert!(resp.cwd.ends_with("/test-repo"));
        assert!(resp.error.is_none());

        // Verify CLAUDE.md was found
        let prompt = resp.exported_system_prompt.unwrap();
        assert_eq!(prompt.file_name, "CLAUDE.md");
        assert!(prompt.content.contains("Test instructions"));

        // Verify skill was found
        assert_eq!(resp.exported_skills.len(), 1);
        assert_eq!(resp.exported_skills[0].name, "review");
        assert!(resp.exported_skills[0]
            .content
            .contains("Review everything"));

        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn initialize_repo_missing_url_fails() {
        let result = initialize_repo("/tmp/sandbox-test-missing", None, None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("repo_url"));
    }

    #[tokio::test]
    async fn initialize_repo_bad_url_fails() {
        if !git_available().await {
            eprintln!("git not available, skipping");
            return;
        }
        let dir = std::env::temp_dir().join("sandbox-test-bad-url");
        let _ = tokio::fs::remove_dir_all(&dir).await;

        let result = initialize_repo(
            dir.to_str().unwrap(),
            Some("https://invalid.example.com/nonexistent/repo.git"),
            None,
        )
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("git clone failed"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── Grep tool ────────────────────────────────────────────────────

    /// Returns true when `rg` is on PATH.
    async fn rg_available() -> bool {
        Command::new("rg")
            .arg("--version")
            .output()
            .await
            .is_ok_and(|o| o.status.success())
    }

    #[tokio::test]
    async fn grep_finds_pattern_in_file() {
        if !rg_available().await {
            eprintln!("rg not available, skipping");
            return;
        }
        let dir = std::env::temp_dir().join("sandbox-test-grep");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(
            dir.join("hello.txt"),
            "hello world\ngoodbye world\nhello rust",
        )
        .await
        .unwrap();

        let input = serde_json::json!({
            "pattern": "hello",
            "path": dir.to_str().unwrap(),
            "output_mode": "content"
        });
        let result = execute_grep(&test_session(), input).await;
        assert!(result.is_ok(), "grep failed: {:?}", result);
        let output = result.unwrap();
        assert!(output.contains("hello world"));
        assert!(output.contains("hello rust"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn grep_files_with_matches_mode() {
        if !rg_available().await {
            eprintln!("rg not available, skipping");
            return;
        }
        let dir = std::env::temp_dir().join("sandbox-test-grep-fwm");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("a.txt"), "match here")
            .await
            .unwrap();
        tokio::fs::write(dir.join("b.txt"), "no hit").await.unwrap();

        let input = serde_json::json!({
            "pattern": "match",
            "path": dir.to_str().unwrap(),
            "output_mode": "files_with_matches"
        });
        let result = execute_grep(&test_session(), input).await.unwrap();
        assert!(result.contains("a.txt"));
        assert!(!result.contains("b.txt"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn grep_no_matches_returns_ok_empty() {
        if !rg_available().await {
            eprintln!("rg not available, skipping");
            return;
        }
        let dir = std::env::temp_dir().join("sandbox-test-grep-empty");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("x.txt"), "nothing relevant")
            .await
            .unwrap();

        let input = serde_json::json!({
            "pattern": "zzz_nonexistent",
            "path": dir.to_str().unwrap()
        });
        let result = execute_grep(&test_session(), input).await;
        assert!(result.is_ok());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn grep_head_limit_caps_output() {
        if !rg_available().await {
            eprintln!("rg not available, skipping");
            return;
        }
        let dir = std::env::temp_dir().join("sandbox-test-grep-head");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("many.txt"), "a\na\na\na\na\na\na\na\na\na")
            .await
            .unwrap();

        let input = serde_json::json!({
            "pattern": "a",
            "path": dir.to_str().unwrap(),
            "output_mode": "content",
            "head_limit": 3
        });
        let result = execute_grep(&test_session(), input).await.unwrap();
        assert_eq!(result.lines().count(), 3);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn grep_missing_pattern_returns_error() {
        let input = serde_json::json!({});
        let result = execute_grep(&test_session(), input).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("pattern"));
    }

    // ── Glob tool ────────────────────────────────────────────────────

    #[tokio::test]
    async fn glob_finds_matching_files() {
        if !rg_available().await {
            eprintln!("rg not available, skipping");
            return;
        }
        let dir = std::env::temp_dir().join("sandbox-test-glob");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("foo.rs"), "fn main() {}")
            .await
            .unwrap();
        tokio::fs::write(dir.join("bar.txt"), "text").await.unwrap();

        let input = serde_json::json!({
            "pattern": "*.rs",
            "path": dir.to_str().unwrap()
        });
        let result = execute_glob(&test_session(), input).await;
        assert!(result.is_ok(), "glob failed: {:?}", result);
        let output = result.unwrap();
        assert!(output.contains("foo.rs"));
        assert!(!output.contains("bar.txt"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn glob_no_matches_returns_ok() {
        if !rg_available().await {
            eprintln!("rg not available, skipping");
            return;
        }
        let dir = std::env::temp_dir().join("sandbox-test-glob-empty");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("hello.txt"), "text")
            .await
            .unwrap();

        let input = serde_json::json!({
            "pattern": "*.xyz",
            "path": dir.to_str().unwrap()
        });
        let result = execute_glob(&test_session(), input).await;
        assert!(result.is_ok());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn glob_missing_pattern_returns_error() {
        let input = serde_json::json!({});
        let result = execute_glob(&test_session(), input).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("pattern"));
    }

    // ── validate_path (Layer 1 isolation) ─────────────────────────

    #[test]
    fn validate_path_allows_workspace_paths() {
        init_test_workspace();
        let workspace = workspace_root();
        let dir = PathBuf::from(&workspace);

        // Ensure workspace dir exists for canonicalization
        std::fs::create_dir_all(&dir).unwrap();
        let test_file = dir.join("test-validate.txt");
        std::fs::write(&test_file, "ok").unwrap();

        let result = validate_path_against(&dir, test_file.to_str().unwrap());
        assert!(result.is_ok(), "expected ok, got: {:?}", result);

        std::fs::remove_file(&test_file).unwrap();
    }

    #[test]
    fn validate_path_rejects_outside_paths() {
        init_test_workspace();
        let workspace = PathBuf::from(workspace_root());
        let result = validate_path_against(&workspace, "/etc/passwd");
        assert!(result.is_err());
        assert!(
            result.as_ref().unwrap_err().contains("Access denied"),
            "expected 'Access denied', got: {:?}",
            result
        );
    }

    #[test]
    fn validate_path_rejects_secrets() {
        init_test_workspace();
        let workspace = PathBuf::from(workspace_root());
        let result = validate_path_against(&workspace, "/run/secrets/github-token");
        assert!(result.is_err());
        // Either "Access denied" (if the path exists) or "Cannot resolve"
        // (if parent doesn't exist). Both are acceptable rejections.
        let err = result.unwrap_err();
        assert!(
            err.contains("Access denied") || err.contains("Cannot resolve"),
            "expected rejection, got: {err}"
        );
    }

    #[test]
    fn validate_path_rejects_traversal() {
        init_test_workspace();
        let workspace = workspace_root();
        std::fs::create_dir_all(&workspace).unwrap();

        // Try to escape workspace via ../
        let traversal = format!("{workspace}/../etc/passwd");
        let result = validate_path_against(&PathBuf::from(&workspace), &traversal);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("Access denied") || err.contains("Cannot resolve"),
            "expected rejection for traversal, got: {err}"
        );
    }

    #[test]
    fn validate_path_handles_new_files() {
        init_test_workspace();
        let workspace = workspace_root();
        std::fs::create_dir_all(&workspace).unwrap();

        // Non-existent file in a valid directory should pass
        let new_file = format!("{workspace}/does-not-exist-yet.txt");
        let result = validate_path_against(&PathBuf::from(&workspace), &new_file);
        assert!(
            result.is_ok(),
            "expected ok for new file, got: {:?}",
            result
        );
    }

    /// Allocates a fresh per-test directory under `temp_dir()` so
    /// credential / reset tests can run in parallel without touching
    /// `WORKSPACE_ROOT` (which other tests read concurrently to resolve
    /// path scopes).
    fn fresh_test_dir(prefix: &str) -> String {
        let dir = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_str().unwrap().to_string()
    }

    /// Wire-shape smoke test: the empty `AttachRequest` deserialises
    /// from `{}` and is the only shape the orchestrator sends post-M4
    /// (no GitHub token field — the proxy handles git auth).
    #[test]
    fn attach_request_default_is_empty() {
        let _req = AttachRequest::default();
        let parsed: AttachRequest = serde_json::from_str(r#"{}"#).unwrap();
        let _ = parsed;
    }

    #[tokio::test]
    async fn reset_repo_skipped_when_cwd_has_no_git_dir() {
        let workspace = fresh_test_dir("sandbox-reset-nogit");
        let session: SharedSession = Arc::new(BashSession::new());
        session.set_cwd(workspace).await;

        let resp = reset_repo(State(session)).await;
        // No `.git` → subshell prints `__reset_skipped_no_git__` and
        // exits 0. `ok` reports the workspace is in the expected
        // baseline state (a no-op skip is a fine outcome).
        assert!(
            resp.0.ok,
            "no-git skip case should be ok=true: {:?}",
            resp.0
        );
        assert!(
            resp.0.output.contains("__reset_skipped_no_git__"),
            "expected skip marker in output: {:?}",
            resp.0.output
        );
        assert!(
            !resp.0.output.contains("__reset_done__"),
            "must not falsely emit reset_done marker: {:?}",
            resp.0.output
        );
    }

    /// Regression test for the "exit 0 kills the persistent shell" bug
    /// (Bugbot finding on PR #286): the reset script's early-return
    /// must run in a subshell so the persistent `BashSession` survives
    /// and a follow-up `execute` doesn't have to respawn.
    #[tokio::test]
    async fn reset_repo_does_not_kill_persistent_shell() {
        let workspace = fresh_test_dir("sandbox-reset-shell-survives");
        let session: SharedSession = Arc::new(BashSession::new());
        session.set_cwd(workspace).await;

        let _ = reset_repo(State(session.clone())).await;

        // If the subshell isolation is broken, this command will
        // either fail with `shell_died` or come back from a brand-new
        // respawned shell. Either way it's a regression.
        let post = session
            .execute("echo persistent_shell_alive", 5_000)
            .await
            .expect("post-reset execute must succeed");
        assert!(
            !post.shell_died,
            "shell must survive reset; stdout={:?} stderr={:?}",
            post.stdout, post.stderr,
        );
        assert!(
            post.stdout.contains("persistent_shell_alive"),
            "follow-up command must run on the same shell; stdout={:?} stderr={:?}",
            post.stdout,
            post.stderr,
        );
    }
}
