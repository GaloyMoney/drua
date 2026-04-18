use std::net::SocketAddr;
use std::path::{Path, PathBuf};
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

#[derive(Deserialize)]
struct InitializeRequest {
    mode: String,
    #[serde(default)]
    repo_url: Option<String>,
    #[serde(default)]
    github_token: Option<String>,
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
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn health() -> &'static str {
    "ok"
}

async fn execute(Json(req): Json<ExecuteRequest>) -> Json<ExecuteResponse> {
    let result = match req.tool.as_str() {
        "bash" => execute_bash(&req.input).await,
        "str_replace_based_edit_tool" => execute_text_editor(&req.input).await,
        "Grep" => execute_grep(&req.input).await,
        "Glob" => execute_glob(&req.input).await,
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
// Bash tool (Anthropic bash_20250124)
//
// Input: { command: string, restart?: bool }
// ---------------------------------------------------------------------------

const DEFAULT_TIMEOUT_MS: u64 = 120_000;

async fn execute_bash(input: &serde_json::Value) -> Result<String, String> {
    // Handle restart: reset is a no-op for a stateless server
    if input
        .get("restart")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Ok("Bash session restarted.".to_string());
    }

    let command = input
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'command' field")?;

    let workspace = workspace_root();
    let workspace_tmp = format!("{workspace}/tmp");
    let _ = tokio::fs::create_dir_all(&workspace_tmp).await;

    // Try bubblewrap first (Layer 3), fall back to uid-only (Layer 2)
    match execute_bash_bwrap(command, &workspace, &workspace_tmp).await {
        Ok(result) => Ok(result),
        Err(bwrap_err) if is_bwrap_unavailable(&bwrap_err) => {
            tracing::warn!("bwrap unavailable, falling back to uid isolation: {bwrap_err}");
            execute_bash_uid_only(command, &workspace, &workspace_tmp).await
        }
        Err(e) => Err(e),
    }
}

/// Layer 3: Execute bash inside a bubblewrap mount namespace.
///
/// Mounts the workspace read-write, Nix store read-only, hides `/run/secrets`
/// behind a tmpfs, and connects to the nix-daemon socket for `nix build` etc.
#[cfg(unix)]
async fn execute_bash_bwrap(
    command: &str,
    workspace: &str,
    workspace_tmp: &str,
) -> Result<String, String> {
    let output = tokio::time::timeout(
        Duration::from_millis(DEFAULT_TIMEOUT_MS),
        Command::new("bwrap")
            // Filesystem mounts
            .args(["--ro-bind", "/nix/store", "/nix/store"])
            .args(["--ro-bind", "/etc", "/etc"])
            .args(["--bind", workspace, workspace])
            .args(["--bind", workspace_tmp, "/tmp"])
            .args(["--tmpfs", "/run"])
            // Nix daemon access
            .args([
                "--bind",
                "/nix/var/nix/daemon-socket",
                "/nix/var/nix/daemon-socket",
            ])
            .args(["--ro-bind", "/nix/var/nix/db", "/nix/var/nix/db"])
            .args(["--ro-bind", "/nix/var/nix/gcroots", "/nix/var/nix/gcroots"])
            .args([
                "--ro-bind",
                "/nix/var/nix/profiles",
                "/nix/var/nix/profiles",
            ])
            // Special filesystems
            .args(["--proc", "/proc"])
            .args(["--dev", "/dev"])
            // Isolation flags
            .args(["--unshare-pid", "--die-with-parent", "--new-session"])
            // UID drop — use bwrap flags (not process-level .uid()/.gid())
            // so that a missing bwrap binary gives "No such file" rather
            // than a pre-exec setuid failure masking the real issue.
            .args(["--uid", "1000", "--gid", "1000"])
            // Environment
            .env("HOME", workspace)
            .env("USER", "agent")
            .env("TMPDIR", "/tmp")
            .env("NIX_REMOTE", "daemon")
            .current_dir(workspace)
            // Execute
            .args(["--", "bash", "-c", command])
            .output(),
    )
    .await
    .map_err(|_| format!("Command timed out after {DEFAULT_TIMEOUT_MS}ms"))?
    .map_err(|e| format!("Failed to execute command: {e}"))?;

    format_bash_output(&output)
}

#[cfg(not(unix))]
async fn execute_bash_bwrap(
    _command: &str,
    _workspace: &str,
    _workspace_tmp: &str,
) -> Result<String, String> {
    Err("bwrap not available on this platform".to_string())
}

/// Layer 2: Execute bash with UID/GID drop only (no mount namespace).
///
/// Used as fallback when bwrap is unavailable (e.g. nested containers without
/// user namespace support). UID drop requires the parent to be root (UID 0);
/// when not root (e.g. in dev/test), runs without privilege drop.
///
/// In fake-root environments (e.g. Nix build sandbox user namespaces), the
/// process appears as UID 0 but cannot actually setuid. When that happens
/// the spawn fails with EPERM ("Operation not permitted") or EINVAL
/// ("Invalid argument" — UID not mapped). We catch both and retry without
/// the UID drop.
#[cfg(unix)]
async fn execute_bash_uid_only(
    command: &str,
    workspace: &str,
    workspace_tmp: &str,
) -> Result<String, String> {
    if is_root() {
        match execute_bash_as_uid(command, workspace, workspace_tmp, Some((1000, 1000))).await {
            Ok(result) => return Ok(result),
            Err(e) if is_uid_drop_error(&e) => {
                tracing::warn!("UID drop failed (fake-root?), falling back to plain bash: {e}");
            }
            Err(e) => return Err(e),
        }
    }

    execute_bash_as_uid(command, workspace, workspace_tmp, None).await
}

/// Detect errors from a failed setuid/setgid in a restricted namespace.
///
/// EPERM  → "Operation not permitted" (no CAP_SETUID)
/// EINVAL → "Invalid argument" (UID not mapped in user namespace)
fn is_uid_drop_error(err: &str) -> bool {
    err.contains("Operation not permitted") || err.contains("Invalid argument")
}

/// Inner helper: run bash with optional UID/GID override.
#[cfg(unix)]
async fn execute_bash_as_uid(
    command: &str,
    workspace: &str,
    workspace_tmp: &str,
    uid_gid: Option<(u32, u32)>,
) -> Result<String, String> {
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(workspace)
        .env("HOME", workspace)
        .env("USER", "agent")
        .env("TMPDIR", workspace_tmp);

    if let Some((uid, gid)) = uid_gid {
        cmd.uid(uid).gid(gid);
    }

    let output = tokio::time::timeout(Duration::from_millis(DEFAULT_TIMEOUT_MS), cmd.output())
        .await
        .map_err(|_| format!("Command timed out after {DEFAULT_TIMEOUT_MS}ms"))?
        .map_err(|e| format!("Failed to execute command: {e}"))?;

    format_bash_output(&output)
}

#[cfg(not(unix))]
async fn execute_bash_uid_only(
    command: &str,
    _workspace: &str,
    _workspace_tmp: &str,
) -> Result<String, String> {
    let output = tokio::time::timeout(
        Duration::from_millis(DEFAULT_TIMEOUT_MS),
        Command::new("bash").arg("-c").arg(command).output(),
    )
    .await
    .map_err(|_| format!("Command timed out after {DEFAULT_TIMEOUT_MS}ms"))?
    .map_err(|e| format!("Failed to execute command: {e}"))?;

    format_bash_output(&output)
}

/// Returns true if the current process is running as root (UID 0).
#[cfg(unix)]
fn is_root() -> bool {
    extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid() is a POSIX function, always safe to call
    unsafe { geteuid() == 0 }
}

/// Detect whether a bwrap error indicates the tool is unavailable vs. a real
/// command failure. Returns `true` for platform/permission issues that warrant
/// a fallback to uid-only isolation.
fn is_bwrap_unavailable(err: &str) -> bool {
    err.contains("No permissions to create new namespace")
        || err.contains("No such file or directory")
        || err.contains("Operation not permitted")
}

fn format_bash_output(output: &std::process::Output) -> Result<String, String> {
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

// ---------------------------------------------------------------------------
// Path validation — Layer 1 isolation for text_editor
//
// Rejects any path that resolves outside the workspace root after symlink
// resolution. For new files that don't exist yet, the parent directory is
// resolved instead.
// ---------------------------------------------------------------------------

fn validate_path(path: &str) -> Result<PathBuf, String> {
    let workspace = PathBuf::from(workspace_root());
    let workspace_canonical =
        std::fs::canonicalize(&workspace).map_err(|e| format!("Cannot resolve workspace: {e}"))?;

    let canonical = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => {
            // File doesn't exist yet — resolve the parent and append filename
            let p = PathBuf::from(path);
            let parent = p
                .parent()
                .ok_or_else(|| format!("Invalid path: no parent directory for '{path}'"))?;
            let parent_canonical = std::fs::canonicalize(parent)
                .map_err(|e| format!("Cannot resolve parent directory: {e}"))?;
            parent_canonical.join(
                p.file_name()
                    .ok_or_else(|| format!("Invalid path: no filename in '{path}'"))?,
            )
        }
    };

    if !canonical.starts_with(&workspace_canonical) {
        return Err(format!(
            "Access denied: path '{}' is outside workspace '{}'",
            path,
            workspace_root()
        ));
    }
    Ok(canonical)
}

// ---------------------------------------------------------------------------
// Text editor tool (Anthropic text_editor_20250728)
//
// Commands: view, create, str_replace, insert
// ---------------------------------------------------------------------------

async fn execute_text_editor(input: &serde_json::Value) -> Result<String, String> {
    let command = input
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'command' field")?;

    match command {
        "view" => editor_view(input).await,
        "create" => editor_create(input).await,
        "str_replace" => editor_str_replace(input).await,
        "insert" => editor_insert(input).await,
        other => Err(format!("Unknown text editor command: {other}")),
    }
}

/// View a file (with optional line range) or list a directory.
async fn editor_view(input: &serde_json::Value) -> Result<String, String> {
    let raw_path = input
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'path' field")?;
    let validated = validate_path(raw_path)?;
    let path = validated.to_str().ok_or("Path is not valid UTF-8")?;

    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("Error: {e}"))?;

    if meta.is_dir() {
        // List directory contents
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
        // Read file contents
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| format!("Error: {e}"))?;
        let lines: Vec<&str> = content.lines().collect();

        let (start, end) = if let Some(range) = input.get("view_range").and_then(|v| v.as_array()) {
            let s = range.first().and_then(|v| v.as_i64()).unwrap_or(1).max(1) as usize;
            let e = range.get(1).and_then(|v| v.as_i64()).unwrap_or(-1);
            let end = if e == -1 {
                lines.len()
            } else {
                (e as usize).min(lines.len())
            };
            (s, end)
        } else {
            (1, lines.len())
        };

        let numbered: Vec<String> = lines[start.saturating_sub(1)..end]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{}: {line}", start + i))
            .collect();

        Ok(numbered.join("\n"))
    }
}

/// Create a new file with the given content.
async fn editor_create(input: &serde_json::Value) -> Result<String, String> {
    let raw_path = input
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'path' field")?;
    let file_text = input
        .get("file_text")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'file_text' field")?;

    // Create parent dirs first so validate_path can canonicalize the parent
    if let Some(parent) = std::path::Path::new(raw_path).parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Error creating directories: {e}"))?;
    }
    let validated = validate_path(raw_path)?;
    let path = validated.to_str().ok_or("Path is not valid UTF-8")?;

    tokio::fs::write(path, file_text)
        .await
        .map_err(|e| format!("Error: {e}"))?;

    Ok(format!("File created successfully at: {path}"))
}

/// Replace exactly one occurrence of `old_str` with `new_str` in a file.
async fn editor_str_replace(input: &serde_json::Value) -> Result<String, String> {
    let raw_path = input
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'path' field")?;
    let validated = validate_path(raw_path)?;
    let path = validated.to_str().ok_or("Path is not valid UTF-8")?;
    let old_str = input
        .get("old_str")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'old_str' field")?;
    let new_str = input
        .get("new_str")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'new_str' field")?;

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

/// Insert text after a given line number (0 = beginning of file).
async fn editor_insert(input: &serde_json::Value) -> Result<String, String> {
    let raw_path = input
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'path' field")?;
    let validated = validate_path(raw_path)?;
    let path = validated.to_str().ok_or("Path is not valid UTF-8")?;
    let insert_line = input
        .get("insert_line")
        .and_then(|v| v.as_u64())
        .ok_or("Missing 'insert_line' field")? as usize;
    let insert_text = input
        .get("new_str")
        .or_else(|| input.get("insert_text"))
        .and_then(|v| v.as_str())
        .ok_or("Missing 'new_str' field")?;

    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| format!("Error: {e}"))?;

    let mut lines: Vec<&str> = content.lines().collect();
    let insert_at = insert_line.min(lines.len());

    // Split the insert text into lines and insert them
    let new_lines: Vec<&str> = insert_text.lines().collect();
    for (i, line) in new_lines.iter().enumerate() {
        lines.insert(insert_at + i, line);
    }

    let new_content = lines.join("\n");
    // Preserve trailing newline if original had one
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

// ---------------------------------------------------------------------------
// Grep tool — content search via ripgrep
//
// Input: { pattern, path?, glob?, type?, output_mode?, -i?, -n?, -A?, -B?,
//          -C?, head_limit?, multiline? }
// ---------------------------------------------------------------------------

async fn execute_grep(input: &serde_json::Value) -> Result<String, String> {
    let pattern = input
        .get("pattern")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'pattern' field")?;

    let mut args: Vec<String> = vec!["--no-heading".to_string(), "--color=never".to_string()];

    // Output mode
    let output_mode = input
        .get("output_mode")
        .and_then(|v| v.as_str())
        .unwrap_or("files_with_matches");

    match output_mode {
        "files_with_matches" => args.push("--files-with-matches".to_string()),
        "count" => args.push("--count".to_string()),
        "content" => {} // default rg output
        other => return Err(format!("Unknown output_mode: {other}")),
    }

    // Case insensitive
    if input.get("-i").and_then(|v| v.as_bool()).unwrap_or(false) {
        args.push("--ignore-case".to_string());
    }

    // Line numbers (only meaningful for content mode)
    if output_mode == "content" {
        let show_line_numbers = input.get("-n").and_then(|v| v.as_bool()).unwrap_or(true);
        if show_line_numbers {
            args.push("--line-number".to_string());
        }
    }

    // Context lines
    if let Some(n) = input.get("-A").and_then(|v| v.as_u64()) {
        args.push(format!("--after-context={n}"));
    }
    if let Some(n) = input.get("-B").and_then(|v| v.as_u64()) {
        args.push(format!("--before-context={n}"));
    }
    if let Some(n) = input.get("-C").and_then(|v| v.as_u64()) {
        args.push(format!("--context={n}"));
    }

    // Multiline
    if input
        .get("multiline")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        args.push("--multiline".to_string());
        args.push("--multiline-dotall".to_string());
    }

    // File type filter
    if let Some(ty) = input.get("type").and_then(|v| v.as_str()) {
        args.push(format!("--type={ty}"));
    }

    // Glob filter
    if let Some(glob) = input.get("glob").and_then(|v| v.as_str()) {
        args.push(format!("--glob={glob}"));
    }

    // Pattern
    args.push("--".to_string());
    args.push(pattern.to_string());

    // Search path
    let search_path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    args.push(search_path.to_string());

    let output = tokio::time::timeout(
        Duration::from_millis(DEFAULT_TIMEOUT_MS),
        Command::new("rg").args(&args).output(),
    )
    .await
    .map_err(|_| format!("Grep timed out after {DEFAULT_TIMEOUT_MS}ms"))?
    .map_err(|e| format!("Failed to execute rg: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // rg exit codes: 0 = matches found, 1 = no matches, 2 = error
    match output.status.code() {
        Some(0) | Some(1) => {
            let mut result = stdout.into_owned();

            // Apply head_limit if specified
            if let Some(limit) = input.get("head_limit").and_then(|v| v.as_u64()) {
                let limit = limit as usize;
                let lines: Vec<&str> = result.lines().take(limit).collect();
                result = lines.join("\n");
            }

            Ok(result)
        }
        _ => Err(format!("rg failed: {stderr}")),
    }
}

// ---------------------------------------------------------------------------
// Glob tool — file pattern matching via ripgrep --files -g
//
// Input: { pattern, path? }
// ---------------------------------------------------------------------------

async fn execute_glob(input: &serde_json::Value) -> Result<String, String> {
    let pattern = input
        .get("pattern")
        .and_then(|v| v.as_str())
        .ok_or("Missing 'pattern' field")?;

    let search_path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");

    // Use `rg --files -g <pattern> <path>` to list matching files.
    // Then sort by mtime (most recent first) using `ls -t`.
    let output = tokio::time::timeout(
        Duration::from_millis(DEFAULT_TIMEOUT_MS),
        Command::new("rg")
            .args([
                "--files",
                "--color=never",
                &format!("--glob={pattern}"),
                search_path,
            ])
            .output(),
    )
    .await
    .map_err(|_| format!("Glob timed out after {DEFAULT_TIMEOUT_MS}ms"))?
    .map_err(|e| format!("Failed to execute rg: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    match output.status.code() {
        Some(0) | Some(1) => Ok(stdout.into_owned()),
        _ => Err(format!("rg --files failed: {stderr}")),
    }
}

// ---------------------------------------------------------------------------
// Initialize endpoint
//
// POST /initialize { mode: "scratch"|"repo", repo_url?: string }
// ---------------------------------------------------------------------------

const DEFAULT_WORKSPACE_ROOT: &str = "/workspace";
const DEFAULT_GITHUB_TOKEN_PATH: &str = "/run/secrets/github-token";

fn workspace_root() -> String {
    std::env::var("WORKSPACE_ROOT").unwrap_or_else(|_| DEFAULT_WORKSPACE_ROOT.to_string())
}

fn github_token_path() -> String {
    std::env::var("GITHUB_TOKEN_PATH").unwrap_or_else(|_| DEFAULT_GITHUB_TOKEN_PATH.to_string())
}

async fn initialize(Json(req): Json<InitializeRequest>) -> Json<InitializeResponse> {
    if let Some(token) = req.github_token.as_deref().filter(|t| !t.is_empty()) {
        if let Err(e) = write_github_token(&github_token_path(), token).await {
            return Json(InitializeResponse {
                cwd: workspace_root(),
                exported_system_prompt: None,
                exported_skills: Vec::new(),
                error: Some(e),
            });
        }
    }

    initialize_inner(&workspace_root(), &req.mode, req.repo_url.as_deref()).await
}

async fn initialize_inner(
    workspace: &str,
    mode: &str,
    repo_url: Option<&str>,
) -> Json<InitializeResponse> {
    let result = match mode {
        "scratch" => initialize_scratch(workspace).await,
        "repo" => initialize_repo(workspace, repo_url).await,
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

/// Write the GitHub token to `path` with mode 0600. Creates parent dirs as needed.
/// The git credential helper baked into the sandbox image reads from this path.
///
/// Also writes workspace-level `.git-credentials` so the UID-dropped agent user
/// can authenticate git operations without access to `/run/secrets/`.
async fn write_github_token(path: &str, token: &str) -> Result<(), String> {
    let path_buf = PathBuf::from(path);
    if let Some(parent) = path_buf.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create token directory {parent:?}: {e}"))?;
    }
    tokio::fs::write(&path_buf, token)
        .await
        .map_err(|e| format!("Failed to write github token: {e}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        tokio::fs::set_permissions(&path_buf, perms)
            .await
            .map_err(|e| format!("Failed to chmod github token: {e}"))?;
    }

    // Write workspace-level git credentials for the agent user (UID 1000)
    write_workspace_git_credentials(token).await?;

    Ok(())
}

/// Write `.git-credentials` into the workspace so the agent user (after UID
/// drop) can push/pull without access to `/run/secrets/`. Also configures git
/// to use the credential store.
async fn write_workspace_git_credentials(token: &str) -> Result<(), String> {
    let workspace = workspace_root();
    let cred_path = format!("{workspace}/.git-credentials");
    let content = format!("https://x-access-token:{token}@github.com\n");

    tokio::fs::write(&cred_path, &content)
        .await
        .map_err(|e| format!("Failed to write git credentials: {e}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&cred_path, std::fs::Permissions::from_mode(0o600))
            .await
            .map_err(|e| format!("Failed to chmod git credentials: {e}"))?;

        // chown to agent:agent (1000:1000) so the UID-dropped process can read it
        let output = std::process::Command::new("chown")
            .args(["1000:1000", &cred_path])
            .output();
        if let Err(e) = output {
            tracing::warn!("Failed to chown git credentials (non-fatal): {e}");
        }
    }

    // Configure git to use the credential store
    let output = std::process::Command::new("git")
        .args([
            "config",
            "--global",
            "credential.helper",
            &format!("store --file={cred_path}"),
        ])
        .output();
    if let Err(e) = output {
        tracing::warn!("Failed to configure git credential store (non-fatal): {e}");
    }

    Ok(())
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

    // Idempotent: skip the clone if the target dir already contains the
    // repo (e.g. after a sandbox restart with a retained workspace). Only
    // considered "already cloned" when `.git` exists inside — a stray empty
    // dir still triggers a fresh clone.
    let already_cloned = clone_dir.join(".git").is_dir();
    if !already_cloned {
        let output = Command::new("git")
            .args(["clone", repo_url, clone_dir.to_str().unwrap()])
            .output()
            .await
            .map_err(|e| format!("Failed to run git clone: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git clone failed: {stderr}"));
        }
    }

    let system_prompt = scan_claude_md(&clone_dir).await;
    let skills = scan_skills(&clone_dir).await;

    Ok(InitializeResponse {
        cwd: clone_dir.to_string_lossy().to_string(),
        exported_system_prompt: system_prompt,
        exported_skills: skills,
        error: None,
    })
}

/// Extract repo name from URL — last path segment, stripped of `.git` suffix.
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

/// Read CLAUDE.md at the repo root, if present.
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

    // Layout 1: .claude/skills/<name>/SKILL.md
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
            if let Ok(content) = tokio::fs::read_to_string(&skill_file).await {
                skills.push(ExportedSkill {
                    name: name.to_string(),
                    content,
                });
            }
        }
    }

    // Layout 2: .claude/commands/*.md (legacy; skipped if the name was
    // already captured from layout 1)
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
                });
            }
        }
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let app = Router::new()
        .route("/health", get(health))
        .route("/execute", post(execute))
        .route("/initialize", post(initialize));

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Set WORKSPACE_ROOT to the system temp dir so that tests using
    /// `std::env::temp_dir()` pass path validation. Called once per process.
    fn init_test_workspace() {
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            let tmp = std::env::temp_dir();
            std::fs::create_dir_all(&tmp).unwrap();
            std::env::set_var("WORKSPACE_ROOT", tmp.to_str().unwrap());
        });
    }

    #[tokio::test]
    async fn bash_executes_echo() {
        init_test_workspace();
        let input = serde_json::json!({"command": "echo hello"});
        let result = execute_bash(&input).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().trim(), "hello");
    }

    #[tokio::test]
    async fn bash_returns_error_on_nonzero_exit() {
        init_test_workspace();
        let input = serde_json::json!({"command": "exit 42"});
        let result = execute_bash(&input).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Exit code 42"));
    }

    #[tokio::test]
    async fn bash_restart_returns_ok() {
        init_test_workspace();
        let input = serde_json::json!({"restart": true});
        let result = execute_bash(&input).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("restarted"));
    }

    #[tokio::test]
    async fn bash_missing_command_returns_error() {
        init_test_workspace();
        let input = serde_json::json!({});
        let result = execute_bash(&input).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("command"));
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
        let result = editor_create(&create_input).await;
        assert!(result.is_ok(), "create failed: {:?}", result);

        let view_input = serde_json::json!({
            "command": "view",
            "path": file.to_str().unwrap()
        });
        let result = editor_view(&view_input).await.unwrap();
        assert!(result.contains("1: line one"));
        assert!(result.contains("2: line two"));
        assert!(result.contains("3: line three"));

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
        editor_create(&create_input).await.unwrap();

        let view_input = serde_json::json!({
            "command": "view",
            "path": file.to_str().unwrap(),
            "view_range": [2, 4]
        });
        let result = editor_view(&view_input).await.unwrap();
        assert!(result.contains("2: b"));
        assert!(result.contains("3: c"));
        assert!(result.contains("4: d"));
        assert!(!result.contains("1: a"));
        assert!(!result.contains("5: e"));

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
        let result = editor_view(&view_input).await.unwrap();
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
        editor_create(&create_input).await.unwrap();

        let replace_input = serde_json::json!({
            "command": "str_replace",
            "path": file.to_str().unwrap(),
            "old_str": "hello world",
            "new_str": "hello rust"
        });
        let result = editor_str_replace(&replace_input).await;
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
        editor_create(&create_input).await.unwrap();

        let replace_input = serde_json::json!({
            "command": "str_replace",
            "path": file.to_str().unwrap(),
            "old_str": "nonexistent",
            "new_str": "replacement"
        });
        let result = editor_str_replace(&replace_input).await;
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
        editor_create(&create_input).await.unwrap();

        let replace_input = serde_json::json!({
            "command": "str_replace",
            "path": file.to_str().unwrap(),
            "old_str": "foo",
            "new_str": "baz"
        });
        let result = editor_str_replace(&replace_input).await;
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
        editor_create(&create_input).await.unwrap();

        let insert_input = serde_json::json!({
            "command": "insert",
            "path": file.to_str().unwrap(),
            "insert_line": 0,
            "new_str": "header line"
        });
        let result = editor_insert(&insert_input).await;
        assert!(result.is_ok(), "insert failed: {:?}", result);

        let content = tokio::fs::read_to_string(&file).await.unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines[0], "header line");
        assert_eq!(lines[1], "line one");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn editor_view_nonexistent_file_returns_error() {
        init_test_workspace();
        let input = serde_json::json!({
            "command": "view",
            "path": "/nonexistent/path/file.txt"
        });
        let result = editor_view(&input).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn text_editor_dispatch_routes_commands() {
        init_test_workspace();
        let dir = std::env::temp_dir().join("sandbox-test-dispatch");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let file = dir.join("dispatch.txt");

        // Test create via dispatch
        let input = serde_json::json!({
            "command": "create",
            "path": file.to_str().unwrap(),
            "file_text": "dispatch test"
        });
        let result = execute_text_editor(&input).await;
        assert!(result.is_ok());

        // Test view via dispatch
        let input = serde_json::json!({
            "command": "view",
            "path": file.to_str().unwrap()
        });
        let result = execute_text_editor(&input).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("dispatch test"));

        // Test unknown command
        let input = serde_json::json!({"command": "delete"});
        let result = execute_text_editor(&input).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
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

    // ── write_github_token ─────────────────────────────────────────

    #[tokio::test]
    async fn write_github_token_creates_file_and_parent_dir() {
        init_test_workspace();
        let base = std::env::temp_dir().join("sandbox-test-token");
        let _ = tokio::fs::remove_dir_all(&base).await;
        let path = base.join("nested").join("github-token");

        write_github_token(path.to_str().unwrap(), "ghp_secret")
            .await
            .unwrap();

        let written = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(written, "ghp_secret");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = tokio::fs::metadata(&path)
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        let _ = tokio::fs::remove_dir_all(&base).await;
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
        let result = initialize_repo("/tmp/sandbox-test-missing", None).await;
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
        let result = execute_grep(&input).await;
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
        let result = execute_grep(&input).await.unwrap();
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
        let result = execute_grep(&input).await;
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
        let result = execute_grep(&input).await.unwrap();
        assert_eq!(result.lines().count(), 3);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn grep_missing_pattern_returns_error() {
        let input = serde_json::json!({});
        let result = execute_grep(&input).await;
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
        let result = execute_glob(&input).await;
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
        let result = execute_glob(&input).await;
        assert!(result.is_ok());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn glob_missing_pattern_returns_error() {
        let input = serde_json::json!({});
        let result = execute_glob(&input).await;
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

        let result = validate_path(test_file.to_str().unwrap());
        assert!(result.is_ok(), "expected ok, got: {:?}", result);

        std::fs::remove_file(&test_file).unwrap();
    }

    #[test]
    fn validate_path_rejects_outside_paths() {
        init_test_workspace();
        let result = validate_path("/etc/passwd");
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
        let result = validate_path("/run/secrets/github-token");
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
        let result = validate_path(&traversal);
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
        let result = validate_path(&new_file);
        assert!(
            result.is_ok(),
            "expected ok for new file, got: {:?}",
            result
        );
    }

    // ── is_bwrap_unavailable ──────────────────────────────────────

    #[test]
    fn bwrap_unavailable_detects_namespace_error() {
        assert!(is_bwrap_unavailable(
            "No permissions to create new namespace"
        ));
    }

    #[test]
    fn bwrap_unavailable_detects_missing_binary() {
        assert!(is_bwrap_unavailable("No such file or directory"));
    }

    #[test]
    fn bwrap_unavailable_detects_operation_not_permitted() {
        assert!(is_bwrap_unavailable("Operation not permitted"));
    }

    #[test]
    fn bwrap_unavailable_returns_false_for_real_errors() {
        assert!(!is_bwrap_unavailable("Exit code 1\ncommand not found: foo"));
    }
}
