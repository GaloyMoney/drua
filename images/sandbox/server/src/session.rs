//! Persistent bash session — keeps a long-lived shell process alive so that
//! background services (postgres, docker-compose, nix services) survive between
//! tool calls.
//!
//! Output is demarcated with a unique per-command marker:
//! `___SANDBOX_EXIT_{request_id}_{exit_code}___`. A `Mutex` serializes commands
//! and the session auto-recovers if the shell dies (stdout EOF).

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::workspace_root;

#[cfg(test)]
use sandbox::BashCommandInput;
#[cfg(test)]
const DEFAULT_TIMEOUT_MS: u64 = BashCommandInput::DEFAULT_TIMEOUT_MS;

const MARKER_PREFIX: &str = "___SANDBOX_EXIT_";
const MARKER_SUFFIX: &str = "___";

/// Echo appended after every user command. Captures `$?` before echo overwrites it.
fn marker_echo(request_id: &str) -> String {
    format!(
        r#"__sandbox_ec=$?; echo ""; echo "{MARKER_PREFIX}{request_id}_${{__sandbox_ec}}{MARKER_SUFFIX}"; "#
    )
}

fn parse_marker(line: &str, request_id: &str) -> Option<i32> {
    let expected_prefix = format!("{MARKER_PREFIX}{request_id}_");
    let stripped = line.strip_prefix(&expected_prefix)?;
    let code_str = stripped.strip_suffix(MARKER_SUFFIX)?;
    code_str.parse::<i32>().ok()
}

/// Spawns the persistent bash session.
///
/// Drops to the agent uid (1000) when the server runs as root in the prod
/// container, falling back to plain bash otherwise (dev, fake-root, or a
/// uid drop that fails its probe). Pod-level isolation (gVisor +
/// NetworkPolicy + ephemeral single-tenant pod) is the load-bearing
/// boundary; the uid drop is a cheap in-pod DAC layer on top of it.
async fn spawn_session_shell(
    workspace: &str,
    workspace_tmp: &str,
    cwd: &str,
) -> Result<Child, String> {
    #[cfg(unix)]
    if is_root() {
        match try_spawn(true, workspace, workspace_tmp, cwd) {
            Ok(mut child) => match probe_child_alive(&mut child).await {
                Ok(()) => {
                    tracing::info!(layer = "uid-only", "spawned session shell");
                    return Ok(child);
                }
                Err((code, stderr)) => {
                    tracing::warn!(
                        layer = "uid-only",
                        exit_code = code,
                        stderr = ?stderr.trim(),
                        "shell exited immediately after spawn, falling back",
                    );
                }
            },
            Err(e) if is_spawn_failure(&e) => {
                tracing::warn!("uid-only session failed (fake-root?), falling back: {e}");
            }
            Err(e) => return Err(e),
        }
    }

    let mut child = try_spawn(false, workspace, workspace_tmp, cwd)?;
    if let Err((code, stderr)) = probe_child_alive(&mut child).await {
        return Err(format!(
            "plain bash exited immediately (code={code}): {}",
            stderr.trim()
        ));
    }
    tracing::info!(layer = "plain", "spawned session shell");
    Ok(child)
}

/// Brief liveness probe for a freshly-spawned child. `Ok(())` means the
/// child is still running after a short window — taken as evidence that
/// argument validation and any startup checks have passed. `Err((code,
/// stderr))` means the child has already exited; the caller should treat
/// the layer as unavailable. The stderr drain is best-effort and only
/// runs in the failure path, so a healthy child's stderr stream is left
/// untouched for the persistent drainer task to claim.
async fn probe_child_alive(child: &mut Child) -> Result<(), (i32, String)> {
    const PROBE_WINDOW: Duration = Duration::from_millis(150);
    const POLL: Duration = Duration::from_millis(10);

    let deadline = std::time::Instant::now() + PROBE_WINDOW;
    loop {
        match child.try_wait() {
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    return Ok(());
                }
                tokio::time::sleep(POLL).await;
            }
            Ok(Some(status)) => {
                let code = status.code().unwrap_or(-1);
                let mut stderr_buf = String::new();
                if let Some(stderr) = child.stderr.as_mut() {
                    let _ = stderr.read_to_string(&mut stderr_buf).await;
                }
                return Err((code, stderr_buf));
            }
            Err(_) => return Err((-1, String::new())),
        }
    }
}

fn try_spawn(
    drop_uid: bool,
    workspace: &str,
    workspace_tmp: &str,
    cwd: &str,
) -> Result<Child, String> {
    let mut cmd = Command::new("bash");
    cmd.args(["--noediting", "--noprofile", "--norc", "-i"]);
    #[cfg(unix)]
    if drop_uid {
        cmd.uid(1000).gid(1000);
    }

    // Detach the spawned shell from the parent's controlling terminal —
    // but only when there IS one to detach from. Without setsid(), local
    // `bash -i` calls `tcsetpgrp` and steals the dev server's TTY
    // foreground pgroup, swallowing Ctrl-C. In prod (gVisor sandbox, no
    // tty) the parent has no controlling terminal anyway — and there
    // adding setsid() makes interactive bash exit immediately on first
    // command for reasons we haven't fully characterized (likely a
    // gVisor + bash-job-control interaction).
    #[cfg(unix)]
    if parent_has_controlling_tty() {
        // SAFETY: setsid(2) is async-signal-safe and only mutates the
        // child's session/pgroup state. No allocations, no locks.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("HOME", workspace)
        .env("USER", "agent")
        .env("TMPDIR", workspace_tmp)
        .env("NIX_REMOTE", "daemon")
        .env("PS1", "")
        .current_dir(cwd);

    // gh CLI reads GH_TOKEN from env. Read from the same canonical
    // location the credential helper uses (/run/secrets/github-token)
    // and inject it into the bash session's env so `gh` works without
    // touching the workspace. The sandbox-server runs as root, so it
    // can always read the token regardless of the chmod/chown applied
    // for the agent user.
    if let Ok(token) = std::fs::read_to_string("/run/secrets/github-token") {
        let trimmed = token.trim();
        if !trimmed.is_empty() {
            cmd.env("GH_TOKEN", trimmed);
        }
    }

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

/// True when the *parent* (this process) has a controlling terminal —
/// i.e. there's a tty whose foreground pgroup the spawned shell could
/// otherwise steal. `tcgetpgrp(0) >= 0` is the standard probe: it
/// returns -1/ENOTTY when stdin isn't a tty AND -1/ENXIO when stdin is
/// a tty but no controlling tty is associated.
#[cfg(unix)]
fn parent_has_controlling_tty() -> bool {
    unsafe { libc::tcgetpgrp(0) >= 0 }
}

fn is_spawn_failure(err: &str) -> bool {
    err.starts_with("Failed to execute command:")
}

struct BashSessionInner {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    stderr_handle: tokio::task::JoinHandle<String>,
    stderr_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
}

pub struct BashSession {
    inner: Mutex<Option<BashSessionInner>>,
    workspace: String,
    workspace_tmp: String,
    /// Where freshly-spawned shells start. `/initialize` and the new
    /// `/reset_cwd` endpoint update this so a per-mode working dir
    /// (e.g. `/workspace/library/spaces/<slug>`) is honoured and a
    /// new agent attaching to a preexisting sandbox doesn't inherit
    /// the previous tenant's `cd`.
    cwd: tokio::sync::RwLock<String>,
}

#[derive(Debug)]
pub struct CommandResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub duration_ms: u64,
    /// True when the shell exited before echoing the per-command marker.
    pub shell_died: bool,
}

impl BashSession {
    pub fn new() -> Self {
        let workspace = workspace_root();
        let workspace_tmp = format!("{workspace}/tmp");
        Self {
            inner: Mutex::new(None),
            workspace: workspace.clone(),
            workspace_tmp,
            cwd: tokio::sync::RwLock::new(workspace),
        }
    }

    /// Record a new starting cwd and drop the active shell so the
    /// next `execute` respawns inside `cwd`. No-op if `cwd` is empty.
    pub async fn set_cwd(&self, cwd: String) {
        if cwd.is_empty() {
            return;
        }
        {
            let mut guard = self.cwd.write().await;
            *guard = cwd;
        }
        // Drop any live shell so the next /execute respawns with the
        // fresh cwd. Drop is best-effort; a half-running shell will
        // be cleaned up naturally on next read failure.
        let mut inner = self.inner.lock().await;
        let _ = inner.take();
    }

    /// Snapshot of the current cwd.
    pub async fn current_cwd(&self) -> String {
        self.cwd.read().await.clone()
    }

    /// Lazily creates the session on first call. On stdin error or stdout EOF,
    /// clears the session so the next call recreates it.
    pub async fn execute(&self, command: &str, timeout_ms: u64) -> Result<CommandResult, String> {
        let mut guard = self.inner.lock().await;

        if guard.is_none() {
            let session = self.create_session().await?;
            *guard = Some(session);
        }

        let session = guard.as_mut().unwrap();
        let request_id = uuid::Uuid::new_v4().to_string();

        let full_command = format!("{command}\n{marker}\n", marker = marker_echo(&request_id),);

        if let Err(e) = session.stdin.write_all(full_command.as_bytes()).await {
            let _ = guard.take();
            return Err(format!("Session stdin write failed (shell died?): {e}"));
        }
        if let Err(e) = session.stdin.flush().await {
            let _ = guard.take();
            return Err(format!("Session stdin flush failed: {e}"));
        }

        let timeout = Duration::from_millis(timeout_ms);
        let started = std::time::Instant::now();
        match tokio::time::timeout(timeout, read_until_marker(&mut session.stdout, &request_id))
            .await
        {
            Ok(Ok(result)) => {
                let mut output = self.collect_output(session, result).await;
                output.duration_ms = started.elapsed().as_millis() as u64;
                Ok(output)
            }
            Ok(Err(read_err)) => {
                let mut session = guard.take().expect("session exists");
                let _ = session.stdin.shutdown().await;
                let exit_code = match session.child.wait().await {
                    Ok(status) => status.code().unwrap_or(1),
                    Err(_) => 1,
                };
                let mut stderr_buf = String::new();
                while let Some(chunk) = session.stderr_rx.recv().await {
                    stderr_buf.push_str(&chunk);
                }
                Ok(CommandResult {
                    stdout: read_err.partial_stdout,
                    stderr: stderr_buf,
                    exit_code,
                    duration_ms: started.elapsed().as_millis() as u64,
                    shell_died: true,
                })
            }
            Err(_) => {
                // Try SIGINT first so background processes survive a foreground timeout.
                let result = Self::try_interrupt_foreground(session, &request_id).await;
                if result.is_err()
                    && result
                        .as_ref()
                        .err()
                        .is_some_and(|e| e.contains("could not be recovered"))
                {
                    let _ = guard.take();
                }
                result
            }
        }
    }

    async fn collect_output(
        &self,
        session: &mut BashSessionInner,
        result: MarkerResult,
    ) -> CommandResult {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let mut stderr_buf = String::new();
        while let Ok(chunk) = session.stderr_rx.try_recv() {
            stderr_buf.push_str(&chunk);
        }
        CommandResult {
            stdout: result.output,
            stderr: stderr_buf,
            exit_code: result.exit_code,
            duration_ms: 0,
            shell_died: false,
        }
    }

    /// Send SIGINT and wait briefly for the marker echo. Falls back to killing
    /// the session if the marker doesn't appear within the grace period.
    async fn try_interrupt_foreground(
        session: &mut BashSessionInner,
        request_id: &str,
    ) -> Result<CommandResult, String> {
        #[cfg(unix)]
        if let Some(pid) = session.child.id() {
            // SAFETY: kill(2) is POSIX, sending SIGINT to a known PID.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGINT);
            }
        }

        const GRACE: Duration = Duration::from_secs(5);
        match tokio::time::timeout(GRACE, read_until_marker(&mut session.stdout, request_id)).await
        {
            Ok(Ok(result)) => {
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
                let _ = session.child.kill().await;
                Err("Command timed out and session could not be recovered".to_string())
            }
        }
    }

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

        let cwd = {
            let cwd_guard = self.cwd.read().await;
            // Fall back to workspace if the recorded cwd no longer
            // exists (e.g. an old space dir was deleted out from
            // under us) — keeps the shell alive instead of failing
            // to spawn.
            if std::path::Path::new(cwd_guard.as_str()).is_dir() {
                cwd_guard.clone()
            } else {
                self.workspace.clone()
            }
        };
        let mut child = spawn_session_shell(&self.workspace, &self.workspace_tmp, &cwd).await?;

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

        // Drain stderr continuously and forward chunks via a channel so the
        // main read loop can collect them when needed.
        let (stderr_tx, stderr_rx) = tokio::sync::mpsc::unbounded_channel();
        let stderr_handle = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut buf = String::new();
            loop {
                buf.clear();
                match reader.read_line(&mut buf).await {
                    Ok(0) => break,
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

pub type SharedSession = Arc<BashSession>;

struct MarkerResult {
    output: String,
    exit_code: i32,
}

/// Returned when the marker never arrives: carries the bytes the shell
/// did manage to print on stdout before EOF or a read error. The execute
/// path uses this to surface partial output instead of dropping it.
struct ReadError {
    partial_stdout: String,
}

async fn read_until_marker(
    reader: &mut BufReader<tokio::process::ChildStdout>,
    request_id: &str,
) -> Result<MarkerResult, ReadError> {
    let mut output = String::new();
    let mut line_buf = String::new();

    loop {
        line_buf.clear();
        match reader.read_line(&mut line_buf).await {
            Ok(0) => {
                return Err(ReadError {
                    partial_stdout: output,
                });
            }
            Ok(_) => {}
            Err(_) => {
                return Err(ReadError {
                    partial_stdout: output,
                });
            }
        }

        let trimmed = line_buf.trim();
        if let Some(exit_code) = parse_marker(trimmed, request_id) {
            return Ok(MarkerResult { output, exit_code });
        }

        output.push_str(&line_buf);
    }
}

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

    /// A still-running child should pass the probe.
    #[tokio::test]
    async fn probe_child_alive_returns_ok_for_running_child() {
        let mut child = Command::new("sleep")
            .arg("1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sleep");

        let result = probe_child_alive(&mut child).await;
        assert!(
            result.is_ok(),
            "running child should pass probe: {result:?}"
        );

        let _ = child.kill().await;
    }

    /// A child that exits with a non-zero code and prints to stderr
    /// before dying should be reported with both pieces of information,
    /// before any caller has read from the pipes.
    #[tokio::test]
    async fn probe_child_alive_captures_exit_code_and_stderr() {
        let mut child = Command::new("bash")
            .args(["-c", "echo spawn-fail-error >&2; exit 7"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn bash");

        let (code, stderr) = probe_child_alive(&mut child)
            .await
            .expect_err("dying child must surface as Err");
        assert_eq!(code, 7);
        assert!(
            stderr.contains("spawn-fail-error"),
            "stderr should be drained, got: {stderr:?}"
        );
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

    /// Regression test for the Ctrl-C-swallowed bug: the persistent
    /// `bash -i` must NOT inherit the parent's session when the parent
    /// owns a controlling tty. Without setsid(), interactive bash takes
    /// over the tty's foreground pgroup and Ctrl-C on the dev server
    /// stops being delivered.
    ///
    /// `setsid()` is only applied when the parent has a controlling tty
    /// (skipping it in container environments like gVisor where `bash -i`
    /// + setsid kills the shell). The test only runs when the parent
    /// does have a tty, since that's the scenario the fix actually targets.
    #[cfg(unix)]
    #[tokio::test]
    async fn session_shell_is_in_a_separate_session() {
        init_test_workspace();
        if !parent_has_controlling_tty() {
            eprintln!(
                "skipping: no parent controlling tty — \
                 setsid() is intentionally not applied here"
            );
            return;
        }

        let session = BashSession::new();

        let _ = session
            .execute("echo trigger-spawn", DEFAULT_TIMEOUT_MS)
            .await
            .expect("spawn shell");

        let inner = session.inner.lock().await;
        let inner = inner.as_ref().expect("session should be live");
        let child_pid = inner.child.id().expect("child has pid") as libc::pid_t;

        // SAFETY: getsid is a pure read of kernel state for a known PID.
        let child_sid = unsafe { libc::getsid(child_pid) };
        let self_sid = unsafe { libc::getsid(0) };

        assert!(child_sid > 0, "getsid(child) failed: {child_sid}");
        assert_ne!(
            child_sid, self_sid,
            "shell session must be detached from the parent's session, \
             otherwise interactive bash steals the controlling TTY's \
             foreground pgroup and Ctrl-C is swallowed"
        );
    }
}
