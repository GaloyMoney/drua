//! Persistent bash session — keeps a long-lived shell process alive so that
//! background services (postgres, docker-compose, nix services) survive between
//! tool calls.
//!
//! ## Design
//!
//! A single bwrap (or uid-only, or plain) bash session is created lazily on the
//! first `/execute` bash call. Subsequent commands are piped through stdin.
//! Output is demarcated with a unique marker:
//!
//! ```text
//! ___SANDBOX_EXIT_{request_id}_{exit_code}___
//! ```
//!
//! A [`tokio::sync::Mutex`] ensures one command at a time. The session
//! auto-recovers if the shell process dies (detected via stdout EOF).

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::workspace_root;

#[cfg(test)]
use crate::DEFAULT_TIMEOUT_MS;

// ---------------------------------------------------------------------------
// Marker protocol
// ---------------------------------------------------------------------------

const MARKER_PREFIX: &str = "___SANDBOX_EXIT_";
const MARKER_SUFFIX: &str = "___";

/// Build the echo command appended after every user command.
fn marker_echo(request_id: &str) -> String {
    // Use a sub-shell to capture $? before echo overwrites it.
    format!(
        r#"__sandbox_ec=$?; echo ""; echo "{MARKER_PREFIX}{request_id}_${{__sandbox_ec}}{MARKER_SUFFIX}"; "#
    )
}

/// Parse exit code from a marker line. Returns `None` if the line is not a marker
/// for the given request id.
fn parse_marker(line: &str, request_id: &str) -> Option<i32> {
    let expected_prefix = format!("{MARKER_PREFIX}{request_id}_");
    let stripped = line.strip_prefix(&expected_prefix)?;
    let code_str = stripped.strip_suffix(MARKER_SUFFIX)?;
    code_str.parse::<i32>().ok()
}

// ---------------------------------------------------------------------------
// Isolation layer — how to spawn the persistent shell
// ---------------------------------------------------------------------------

/// Describes which isolation layer to use when spawning the session.
#[derive(Debug, Clone, Copy)]
enum IsolationLayer {
    /// Full bubblewrap with mount namespace, PID namespace, uid drop.
    Bwrap,
    /// UID/GID drop only (no mount namespace).
    UidOnly,
    /// No isolation — plain bash.
    Plain,
}

/// Try each isolation layer in order until one works.
async fn spawn_session_shell(workspace: &str, workspace_tmp: &str) -> Result<Child, String> {
    // Layer 3: bwrap
    #[cfg(unix)]
    match try_spawn(IsolationLayer::Bwrap, workspace, workspace_tmp) {
        Ok(child) => return Ok(child),
        Err(e) if is_bwrap_unavailable(&e) => {
            tracing::warn!("bwrap unavailable for session, falling back: {e}");
        }
        Err(e) => return Err(e),
    }

    // Layer 2: uid-only
    #[cfg(unix)]
    if is_root() {
        match try_spawn(IsolationLayer::UidOnly, workspace, workspace_tmp) {
            Ok(child) => return Ok(child),
            Err(e) if is_spawn_failure(&e) => {
                tracing::warn!("uid-only session failed (fake-root?), falling back: {e}");
            }
            Err(e) => return Err(e),
        }
    }

    // Layer 1: plain bash
    try_spawn(IsolationLayer::Plain, workspace, workspace_tmp)
}

fn try_spawn(layer: IsolationLayer, workspace: &str, workspace_tmp: &str) -> Result<Child, String> {
    let mut cmd = match layer {
        IsolationLayer::Bwrap => {
            let mut c = Command::new("bwrap");
            c.args(["--ro-bind", "/nix/store", "/nix/store"])
                .args(["--ro-bind", "/etc", "/etc"])
                .args(["--bind", workspace, workspace])
                .args(["--bind", workspace_tmp, "/tmp"])
                .args(["--tmpfs", "/run"])
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
                .args(["--proc", "/proc"])
                .args(["--dev", "/dev"])
                .args(["--unshare-pid", "--die-with-parent", "--new-session"])
                .args(["--uid", "1000", "--gid", "1000"])
                .args(["--", "bash", "--noediting", "--noprofile", "--norc", "-i"]);
            c
        }
        IsolationLayer::UidOnly => {
            let mut c = Command::new("bash");
            c.args(["--noediting", "--noprofile", "--norc", "-i"]);
            #[cfg(unix)]
            {
                c.uid(1000).gid(1000);
            }
            c
        }
        IsolationLayer::Plain => {
            let mut c = Command::new("bash");
            c.args(["--noediting", "--noprofile", "--norc", "-i"]);
            c
        }
    };

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("HOME", workspace)
        .env("USER", "agent")
        .env(
            "TMPDIR",
            if matches!(layer, IsolationLayer::Bwrap) {
                "/tmp"
            } else {
                workspace_tmp
            },
        )
        .env("NIX_REMOTE", "daemon")
        .env("PS1", "") // suppress prompt noise
        .current_dir(workspace);

    cmd.spawn()
        .map_err(|e| format!("Failed to execute command: {e}"))
}

#[cfg(unix)]
fn is_root() -> bool {
    extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { geteuid() == 0 }
}

fn is_bwrap_unavailable(err: &str) -> bool {
    err.contains("No permissions to create new namespace")
        || err.contains("No such file or directory")
        || err.contains("Operation not permitted")
}

fn is_spawn_failure(err: &str) -> bool {
    err.starts_with("Failed to execute command:")
}

// ---------------------------------------------------------------------------
// BashSession
// ---------------------------------------------------------------------------

/// A persistent bash shell session.
struct BashSessionInner {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    /// stderr is merged by reading it asynchronously and appending to output.
    stderr_handle: tokio::task::JoinHandle<String>,
    /// Receives stderr chunks collected by the background task.
    stderr_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
}

/// Thread-safe handle to the persistent session.
pub struct BashSession {
    inner: Mutex<Option<BashSessionInner>>,
    workspace: String,
    workspace_tmp: String,
}

/// Result of executing a command in the session.
#[derive(Debug)]
pub struct CommandResult {
    pub output: String,
    pub exit_code: i32,
}

impl BashSession {
    pub fn new() -> Self {
        let workspace = workspace_root();
        let workspace_tmp = format!("{workspace}/tmp");
        Self {
            inner: Mutex::new(None),
            workspace,
            workspace_tmp,
        }
    }

    /// Execute a command in the persistent session.
    ///
    /// Creates the session lazily on first call. If the session is dead,
    /// returns an error and clears the session so the next call recreates it.
    pub async fn execute(&self, command: &str, timeout_ms: u64) -> Result<CommandResult, String> {
        let mut guard = self.inner.lock().await;

        // Ensure session exists
        if guard.is_none() {
            let session = self.create_session().await?;
            *guard = Some(session);
        }

        let session = guard.as_mut().unwrap();
        let request_id = uuid::Uuid::new_v4().to_string();

        // Build the full command: run user command, then echo marker with exit code
        let full_command = format!("{command}\n{marker}\n", marker = marker_echo(&request_id),);

        // Write command to stdin
        if let Err(e) = session.stdin.write_all(full_command.as_bytes()).await {
            // Shell died — clear session for next call to recreate
            let _ = guard.take();
            return Err(format!("Session stdin write failed (shell died?): {e}"));
        }
        if let Err(e) = session.stdin.flush().await {
            let _ = guard.take();
            return Err(format!("Session stdin flush failed: {e}"));
        }

        // Read stdout until marker appears, with timeout
        let timeout = Duration::from_millis(timeout_ms);
        match tokio::time::timeout(timeout, read_until_marker(&mut session.stdout, &request_id))
            .await
        {
            Ok(Ok(result)) => {
                let output = self.collect_output(session, result);
                Ok(output)
            }
            Ok(Err(_)) => {
                // stdout EOF — shell died. Retrieve exit code from the child
                // process so `exit N` still reports the correct code.
                let mut session = guard.take().expect("session exists");
                let exit_code = match session.child.wait().await {
                    Ok(status) => status.code().unwrap_or(1),
                    Err(_) => 1,
                };
                Ok(CommandResult {
                    output: String::new(),
                    exit_code,
                })
            }
            Err(_) => {
                // Timeout — try SIGINT first to interrupt the foreground command
                // while keeping the session (and background processes) alive.
                // The shell will continue to the marker echo after SIGINT.
                let result = Self::try_interrupt_foreground(session, &request_id).await;
                if result.is_err()
                    && result
                        .as_ref()
                        .err()
                        .is_some_and(|e| e.contains("could not be recovered"))
                {
                    // SIGINT didn't work — session is dead, clear it
                    let _ = guard.take();
                }
                result
            }
        }
    }

    /// Collect stdout output and any buffered stderr into a single [`CommandResult`].
    fn collect_output(
        &self,
        session: &mut BashSessionInner,
        result: MarkerResult,
    ) -> CommandResult {
        let mut stderr_buf = String::new();
        while let Ok(chunk) = session.stderr_rx.try_recv() {
            stderr_buf.push_str(&chunk);
        }

        let output = if stderr_buf.is_empty() {
            result.output
        } else {
            format!(
                "{}\n--- stderr ---\n{}",
                result.output.trim_end(),
                stderr_buf
            )
        };

        CommandResult {
            output,
            exit_code: result.exit_code,
        }
    }

    /// Send SIGINT to interrupt the foreground command without killing the
    /// session. After SIGINT, bash continues to the marker echo so we can
    /// recover the output boundary. Falls back to killing the session if the
    /// marker doesn't appear within a grace period.
    async fn try_interrupt_foreground(
        session: &mut BashSessionInner,
        request_id: &str,
    ) -> Result<CommandResult, String> {
        // Send SIGINT to the shell child's process group. Inside the
        // bwrap/bash session this propagates to the foreground job.
        #[cfg(unix)]
        if let Some(pid) = session.child.id() {
            // SAFETY: kill(2) is POSIX, sending SIGINT to a known PID.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGINT);
            }
        }

        // Grace period: wait up to 5 s for the marker after SIGINT.
        const GRACE: Duration = Duration::from_secs(5);
        match tokio::time::timeout(GRACE, read_until_marker(&mut session.stdout, request_id)).await
        {
            Ok(Ok(result)) => {
                // SIGINT worked — the shell survived and produced the marker.
                // Drain stderr and return the (partial) output.
                let mut stderr_buf = String::new();
                while let Ok(chunk) = session.stderr_rx.try_recv() {
                    stderr_buf.push_str(&chunk);
                }
                let mut output = result.output;
                if !stderr_buf.is_empty() {
                    output = format!("{}\n--- stderr ---\n{}", output.trim_end(), stderr_buf);
                }
                Err(format!(
                    "Command timed out (interrupted, session preserved)\n{output}"
                ))
            }
            Ok(Err(_)) | Err(_) => {
                // Shell didn't recover — kill it so the next call recreates.
                let _ = session.child.kill().await;
                Err("Command timed out and session could not be recovered".to_string())
            }
        }
    }

    /// Kill and recreate the session.
    pub async fn restart(&self) -> Result<(), String> {
        let mut guard = self.inner.lock().await;
        if let Some(mut session) = guard.take() {
            let _ = session.child.kill().await;
        }
        let session = self.create_session().await?;
        *guard = Some(session);
        Ok(())
    }

    async fn create_session(&self) -> Result<BashSessionInner, String> {
        let _ = tokio::fs::create_dir_all(&self.workspace).await;
        let _ = tokio::fs::create_dir_all(&self.workspace_tmp).await;

        let mut child = spawn_session_shell(&self.workspace, &self.workspace_tmp).await?;

        let stdin = child
            .stdin
            .take()
            .ok_or("Failed to capture session stdin")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("Failed to capture session stdout")?;
        let stderr = child
            .stderr
            .take()
            .ok_or("Failed to capture session stderr")?;

        let stdout = BufReader::new(stdout);

        // Spawn a background task to drain stderr continuously.
        // We use a channel to forward chunks so the main read loop can
        // collect them when needed.
        let (stderr_tx, stderr_rx) = tokio::sync::mpsc::unbounded_channel();
        let stderr_handle = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut buf = String::new();
            loop {
                buf.clear();
                match reader.read_line(&mut buf).await {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        let _ = stderr_tx.send(buf.clone());
                    }
                    Err(_) => break,
                }
            }
            String::new()
        });

        Ok(BashSessionInner {
            child,
            stdin,
            stdout,
            stderr_handle,
            stderr_rx,
        })
    }
}

impl Drop for BashSessionInner {
    fn drop(&mut self) {
        self.stderr_handle.abort();
    }
}

/// Shared session state accessible via axum's State extractor.
pub type SharedSession = Arc<BashSession>;

// ---------------------------------------------------------------------------
// Read until marker
// ---------------------------------------------------------------------------

struct MarkerResult {
    output: String,
    exit_code: i32,
}

async fn read_until_marker(
    reader: &mut BufReader<tokio::process::ChildStdout>,
    request_id: &str,
) -> Result<MarkerResult, String> {
    let mut output = String::new();
    let mut line_buf = String::new();

    loop {
        line_buf.clear();
        let n = reader
            .read_line(&mut line_buf)
            .await
            .map_err(|e| format!("Failed to read session stdout: {e}"))?;

        if n == 0 {
            // EOF — shell died
            return Err("Shell process exited (stdout EOF)".to_string());
        }

        let trimmed = line_buf.trim();
        if let Some(exit_code) = parse_marker(trimmed, request_id) {
            return Ok(MarkerResult { output, exit_code });
        }

        output.push_str(&line_buf);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Set WORKSPACE_ROOT to the system temp dir so tests pass path validation.
    fn init_test_workspace() {
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            let tmp = std::env::temp_dir();
            std::fs::create_dir_all(&tmp).unwrap();
            std::env::set_var("WORKSPACE_ROOT", tmp.to_str().unwrap());
        });
    }

    #[test]
    fn test_marker_echo_format() {
        let echo = marker_echo("abc-123");
        assert!(echo.contains("___SANDBOX_EXIT_abc-123_"));
        assert!(echo.contains("__sandbox_ec"));
    }

    #[test]
    fn test_parse_marker_valid() {
        let line = "___SANDBOX_EXIT_req-1_0___";
        assert_eq!(parse_marker(line, "req-1"), Some(0));

        let line = "___SANDBOX_EXIT_req-1_42___";
        assert_eq!(parse_marker(line, "req-1"), Some(42));

        let line = "___SANDBOX_EXIT_req-1_127___";
        assert_eq!(parse_marker(line, "req-1"), Some(127));
    }

    #[test]
    fn test_parse_marker_wrong_request_id() {
        let line = "___SANDBOX_EXIT_req-2_0___";
        assert_eq!(parse_marker(line, "req-1"), None);
    }

    #[test]
    fn test_parse_marker_not_a_marker() {
        assert_eq!(parse_marker("hello world", "req-1"), None);
        assert_eq!(parse_marker("", "req-1"), None);
    }

    #[tokio::test]
    async fn session_executes_echo() {
        init_test_workspace();
        let session = BashSession::new();
        let result = session
            .execute("echo hello-session", DEFAULT_TIMEOUT_MS)
            .await;
        assert!(result.is_ok(), "execute failed: {:?}", result.err());
        let r = result.unwrap();
        assert_eq!(r.exit_code, 0);
        assert!(r.output.contains("hello-session"));
    }

    #[tokio::test]
    async fn session_captures_exit_code() {
        init_test_workspace();
        let session = BashSession::new();
        // Use a sub-shell to get non-zero exit without killing the session shell.
        let result = session
            .execute("bash -c 'exit 42'", DEFAULT_TIMEOUT_MS)
            .await;
        assert!(result.is_ok(), "execute failed: {:?}", result.err());
        assert_eq!(result.unwrap().exit_code, 42);
    }

    #[tokio::test]
    async fn session_preserves_state_between_commands() {
        init_test_workspace();
        let session = BashSession::new();

        // Set a variable
        let r = session
            .execute("export MY_VAR=persistent", DEFAULT_TIMEOUT_MS)
            .await
            .unwrap();
        assert_eq!(r.exit_code, 0);

        // Read it back
        let r = session
            .execute("echo $MY_VAR", DEFAULT_TIMEOUT_MS)
            .await
            .unwrap();
        assert_eq!(r.exit_code, 0);
        assert!(r.output.contains("persistent"));
    }

    #[tokio::test]
    async fn session_background_process_survives() {
        init_test_workspace();
        let session = BashSession::new();

        // Start a background process that writes to a temp file
        let tmp = std::env::temp_dir().join("sandbox-session-bg-test");
        let _ = tokio::fs::remove_file(&tmp).await;

        let cmd = format!("(sleep 0.2 && echo bg-alive > {}) &", tmp.to_str().unwrap());
        let r = session.execute(&cmd, DEFAULT_TIMEOUT_MS).await.unwrap();
        assert_eq!(r.exit_code, 0);

        // Run another command (the bg process should still be running)
        let r = session
            .execute("echo foreground", DEFAULT_TIMEOUT_MS)
            .await
            .unwrap();
        assert_eq!(r.exit_code, 0);

        // Wait for background to complete
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Verify the background process wrote its file
        let content = tokio::fs::read_to_string(&tmp).await;
        assert!(
            content.is_ok(),
            "Background process should have survived: {:?}",
            content.err()
        );
        assert!(content.unwrap().contains("bg-alive"));

        let _ = tokio::fs::remove_file(&tmp).await;
    }

    #[tokio::test]
    async fn session_restart_works() {
        init_test_workspace();
        let session = BashSession::new();

        // Set a variable
        let r = session
            .execute("export RESTART_VAR=before", DEFAULT_TIMEOUT_MS)
            .await
            .unwrap();
        assert_eq!(r.exit_code, 0);

        // Restart
        session.restart().await.unwrap();

        // Variable should be gone
        let r = session
            .execute("echo \"${RESTART_VAR:-empty}\"", DEFAULT_TIMEOUT_MS)
            .await
            .unwrap();
        assert!(
            r.output.contains("empty"),
            "variable should be cleared after restart"
        );
    }

    #[tokio::test]
    async fn session_recovers_after_shell_death() {
        init_test_workspace();
        let session = BashSession::new();

        // Kill the shell — `exit 0` causes EOF on stdout.
        // The session detects this and returns the child's exit code.
        let result = session.execute("exit 0", DEFAULT_TIMEOUT_MS).await;
        assert!(
            result.is_ok(),
            "exit should return Ok with code: {:?}",
            result
        );
        assert_eq!(result.unwrap().exit_code, 0);

        // Next call should auto-create a new session
        let r = session.execute("echo recovered", DEFAULT_TIMEOUT_MS).await;
        assert!(r.is_ok(), "should recover: {:?}", r.err());
        assert!(r.unwrap().output.contains("recovered"));
    }

    #[tokio::test]
    async fn session_timeout_returns_error_and_recovers() {
        init_test_workspace();
        let session = BashSession::new();

        // Run a command that will block longer than timeout.
        // SIGINT may or may not interrupt `sleep` depending on the platform,
        // but either way we get a timeout error and can recover.
        let result = session.execute("sleep 30", 200).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("timed out"),
            "expected timeout error, got: {err}"
        );

        // Session should auto-recover on next call
        let r = session
            .execute("echo after-timeout", DEFAULT_TIMEOUT_MS)
            .await;
        assert!(r.is_ok(), "should recover after timeout: {:?}", r.err());
        assert!(r.unwrap().output.contains("after-timeout"));
    }

    #[tokio::test]
    async fn session_timeout_preserves_session_via_sigint() {
        init_test_workspace();
        let session = BashSession::new();

        // Start a background process, then run a command that times out.
        // After SIGINT recovery, the background process should still be alive.
        let tmp = std::env::temp_dir().join("sandbox-session-sigint-test");
        let _ = tokio::fs::remove_file(&tmp).await;

        // Start a background writer
        let bg_cmd = format!(
            "(sleep 0.5 && echo sigint-survived > {}) &",
            tmp.to_str().unwrap()
        );
        let r = session.execute(&bg_cmd, DEFAULT_TIMEOUT_MS).await.unwrap();
        assert_eq!(r.exit_code, 0);

        // This will time out — use a trap so SIGINT is handled gracefully
        // by bash, which lets the marker echo execute.
        let result = session.execute("trap 'true' INT; sleep 30", 200).await;
        assert!(result.is_err());
        let err = result.unwrap_err();

        // If SIGINT worked, the error says "interrupted, session preserved".
        // If it didn't (platform-dependent), the session is killed and
        // recreated on the next call. Either way, we can continue.
        if err.contains("session preserved") {
            // Wait for background process to finish
            tokio::time::sleep(Duration::from_millis(800)).await;

            // Background process should have survived the SIGINT
            let content = tokio::fs::read_to_string(&tmp).await;
            assert!(
                content.is_ok(),
                "Background process should survive SIGINT: {:?}",
                content.err()
            );
            assert!(content.unwrap().contains("sigint-survived"));
        }

        // Either way, next command should work
        let r = session
            .execute("echo still-alive", DEFAULT_TIMEOUT_MS)
            .await;
        assert!(r.is_ok(), "should work after timeout: {:?}", r.err());
        assert!(r.unwrap().output.contains("still-alive"));

        let _ = tokio::fs::remove_file(&tmp).await;
    }
}
