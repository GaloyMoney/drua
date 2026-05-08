//! CGI subprocess wrapper for `git http-backend` (memo §2.2 Pattern A).
//!
//! axum hands us the request body bytes + headers; we spawn
//! `git http-backend` with the right env vars, pipe the body to its
//! stdin, and parse the CGI response (status + headers + body) back
//! out of its stdout. Standalone from drua-core so it's unit-testable
//! against fixture bare repos.

use std::io;
use std::path::Path;
use std::process::Stdio;

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tracing::{debug, instrument};

#[derive(Debug, Error)]
pub enum CgiError {
    #[error("CgiError - Spawn: {0}")]
    Spawn(#[source] io::Error),
    #[error("CgiError - WriteStdin: {0}")]
    WriteStdin(#[source] io::Error),
    #[error("CgiError - ReadStdout: {0}")]
    ReadStdout(#[source] io::Error),
    #[error("CgiError - InvalidStatus: {0}")]
    InvalidStatus(String),
    #[error("CgiError - InvalidHeader: {0}")]
    InvalidHeader(String),
    #[error("CgiError - SubprocessFailed: status={status:?}, stderr={stderr}")]
    SubprocessFailed { status: Option<i32>, stderr: String },
}

/// Inputs the caller pulls off the axum `Request`.
pub struct CgiRequest<'a> {
    pub method: &'a str,
    /// Path under the mirror as `git http-backend` expects it. For
    /// `info/refs` this is `/info/refs`; for `git-upload-pack` it's
    /// `/git-upload-pack`. Caller strips the `/git/<owner>/<repo>.git`
    /// prefix from the request URL before calling.
    pub path_info: &'a str,
    pub query_string: &'a str,
    pub content_type: Option<&'a str>,
    pub content_encoding: Option<&'a str>,
    pub body: Bytes,
}

pub struct CgiResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// Spawns `git http-backend` against the given mirror path and runs the
/// CGI exchange. Times out via the caller's overall request timeout
/// (axum-side); this fn doesn't add its own.
#[instrument(name = "git_proxy.cgi.spawn", skip_all, fields(path_info = %req.path_info, body_bytes = req.body.len()))]
pub async fn spawn_http_backend(
    mirror_path: &Path,
    req: &CgiRequest<'_>,
) -> Result<CgiResponse, CgiError> {
    let mut cmd = Command::new("git");
    cmd.arg("http-backend");
    cmd.env_clear();
    // Per git-http-backend(1): GIT_PROJECT_ROOT + GIT_HTTP_EXPORT_ALL
    // makes the bare repo at root act as the served namespace. We
    // pass the parent dir as ROOT and the bare's basename as the
    // last segment of PATH_INFO, matching how `git clone` shapes URLs.
    let parent = mirror_path
        .parent()
        .expect("mirror path has parent")
        .to_path_buf();
    let basename = mirror_path
        .file_name()
        .expect("mirror path has basename")
        .to_string_lossy()
        .into_owned();
    let path_info = format!("/{basename}{}", req.path_info);

    cmd.env("GIT_PROJECT_ROOT", &parent);
    cmd.env("GIT_HTTP_EXPORT_ALL", "1");
    cmd.env("PATH_INFO", &path_info);
    cmd.env("QUERY_STRING", req.query_string);
    cmd.env("REQUEST_METHOD", req.method);
    cmd.env("CONTENT_LENGTH", req.body.len().to_string());
    // git http-backend treats unauthenticated callers as read-only,
    // refusing receive-pack. We've already authenticated upstream
    // (auth_middleware → AuthSubject::Agent → allow-list); set
    // REMOTE_USER so http-backend enables receive-pack. The value is
    // not used for upstream attribution (the App-installation push
    // handles that); it's just the trigger for "auth'd → write OK".
    cmd.env("REMOTE_USER", "drua-git-proxy");
    if let Some(ct) = req.content_type {
        cmd.env("CONTENT_TYPE", ct);
    }
    if let Some(ce) = req.content_encoding {
        cmd.env("HTTP_CONTENT_ENCODING", ce);
    }
    // git-http-backend needs PATH for sub-tools (git-upload-pack etc.)
    // We can't env_clear and then drop PATH or it'll fail to find them.
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }

    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(CgiError::Spawn)?;

    // Write request body to stdin in a separate task; the subprocess
    // may start streaming stdout before stdin is drained on large
    // packfiles, so we can't sequentialise.
    let body_bytes = req.body.clone();
    let mut stdin = child.stdin.take().expect("piped stdin");
    let write_task = tokio::spawn(async move {
        if !body_bytes.is_empty() {
            stdin.write_all(&body_bytes).await?;
        }
        stdin.shutdown().await?;
        Ok::<(), io::Error>(())
    });

    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut buf = Vec::with_capacity(64 * 1024);
    stdout
        .read_to_end(&mut buf)
        .await
        .map_err(CgiError::ReadStdout)?;

    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut stderr_buf = String::new();
    let _ = stderr.read_to_string(&mut stderr_buf).await;
    let _ = write_task.await;

    let exit = child.wait().await.map_err(|e| CgiError::SubprocessFailed {
        status: None,
        stderr: format!("wait: {e}"),
    })?;
    if !exit.success() {
        return Err(CgiError::SubprocessFailed {
            status: exit.code(),
            stderr: stderr_buf,
        });
    }
    if !stderr_buf.is_empty() {
        debug!(stderr = %stderr_buf, "git http-backend stderr (non-fatal)");
    }

    parse_cgi_response(buf)
}

/// CGI response is `headers\r\n\r\nbody` (or `\n\n` per RFC 3875). The
/// `Status:` header — if present — sets the HTTP status; default 200.
fn parse_cgi_response(raw: Vec<u8>) -> Result<CgiResponse, CgiError> {
    let split = find_header_terminator(&raw).ok_or_else(|| {
        CgiError::InvalidHeader("no \\r\\n\\r\\n separator in CGI response".into())
    })?;
    let header_bytes = &raw[..split.start];
    let body_offset = split.end;

    let mut status = StatusCode::OK;
    let mut headers = HeaderMap::new();
    let header_str = std::str::from_utf8(header_bytes)
        .map_err(|e| CgiError::InvalidHeader(format!("non-utf8 header: {e}")))?;

    for line in header_str.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| CgiError::InvalidHeader(format!("bad header line: {line}")))?;
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("Status") {
            let code_str = value.split_whitespace().next().unwrap_or(value);
            let code: u16 = code_str
                .parse()
                .map_err(|_| CgiError::InvalidStatus(value.to_string()))?;
            status =
                StatusCode::from_u16(code).map_err(|e| CgiError::InvalidStatus(e.to_string()))?;
        } else {
            let header_name = name
                .parse::<HeaderName>()
                .map_err(|e| CgiError::InvalidHeader(format!("bad header name '{name}': {e}")))?;
            let header_value = HeaderValue::from_str(value)
                .map_err(|e| CgiError::InvalidHeader(format!("bad header value '{value}': {e}")))?;
            headers.append(header_name, header_value);
        }
    }

    Ok(CgiResponse {
        status,
        headers,
        body: Bytes::copy_from_slice(&raw[body_offset..]),
    })
}

struct Range {
    start: usize,
    end: usize,
}

fn find_header_terminator(raw: &[u8]) -> Option<Range> {
    // RFC 3875 allows either CRLF CRLF or LF LF separators.
    if let Some(idx) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
        return Some(Range {
            start: idx,
            end: idx + 4,
        });
    }
    if let Some(idx) = raw.windows(2).position(|w| w == b"\n\n") {
        return Some(Range {
            start: idx,
            end: idx + 2,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_cgi_response() {
        let raw = b"Content-Type: text/plain\r\n\r\nhello".to_vec();
        let resp = parse_cgi_response(raw).unwrap();
        assert_eq!(resp.status, StatusCode::OK);
        assert_eq!(resp.headers.get("content-type").unwrap(), "text/plain");
        assert_eq!(&resp.body[..], b"hello");
    }

    #[test]
    fn parses_status_header_overrides_default() {
        let raw = b"Status: 404 Not Found\r\nContent-Type: text/plain\r\n\r\nnope".to_vec();
        let resp = parse_cgi_response(raw).unwrap();
        assert_eq!(resp.status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn parses_lf_lf_separator() {
        let raw = b"Content-Type: x/y\n\nbody".to_vec();
        let resp = parse_cgi_response(raw).unwrap();
        assert_eq!(&resp.body[..], b"body");
    }

    #[test]
    fn rejects_response_without_separator() {
        let raw = b"Content-Type: x/y".to_vec();
        assert!(matches!(
            parse_cgi_response(raw),
            Err(CgiError::InvalidHeader(_))
        ));
    }

    #[test]
    fn rejects_invalid_status() {
        let raw = b"Status: not-a-number\r\n\r\n".to_vec();
        assert!(matches!(
            parse_cgi_response(raw),
            Err(CgiError::InvalidStatus(_))
        ));
    }

    #[test]
    fn handles_multiple_header_values() {
        let raw = b"Set-Cookie: a=1\r\nSet-Cookie: b=2\r\n\r\n".to_vec();
        let resp = parse_cgi_response(raw).unwrap();
        let cookies: Vec<_> = resp.headers.get_all("set-cookie").iter().collect();
        assert_eq!(cookies.len(), 2);
    }
}
