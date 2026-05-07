//! Parses the leading frames of a `git-receive-pack` request body to
//! extract the per-ref command list (`<old-oid> <new-oid> <ref-name>`)
//! BEFORE spawning `git http-backend`.
//!
//! Memo §5.2 calls this "Pattern B streaming-reject" — apply the
//! ref-pattern policy in-process so a denied push never lands on disk
//! at all. Uses `gix-packetline` (blocking-io feature) since axum has
//! already buffered the whole body for us.
//!
//! Wire format reference: <https://git-scm.com/docs/pack-protocol#_reference_update_request_and_packfile_transfer>
//! ```text
//!   pkt-line( "<old> <new> <ref>\0<capabilities>\n" )    -- first cmd
//!   pkt-line( "<old> <new> <ref>\n" )                    -- subsequent cmds
//!   ...
//!   flush-pkt
//!   <PACK ...>                                           -- packfile
//! ```

use std::io::Cursor;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::debug;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefUpdate {
    pub old_oid: String,
    pub new_oid: String,
    pub ref_name: String,
}

#[derive(Debug, Error)]
pub enum PktLineError {
    #[error("PktLineError - Io: {0}")]
    Io(#[from] std::io::Error),
    #[error("PktLineError - Decode: {0}")]
    Decode(#[from] gix_packetline::decode::Error),
    #[error("PktLineError - MalformedCommand: {0}")]
    MalformedCommand(String),
    #[error("PktLineError - NoCommands")]
    NoCommands,
}

/// Parses every `<old> <new> <ref>` command up to the first flush-pkt.
/// Returns the unique ref names in the order they appeared. Capabilities
/// (everything after the first NUL) are stripped.
pub fn parse_command_list(body: &[u8]) -> Result<Vec<RefUpdate>, PktLineError> {
    let mut cursor = Cursor::new(body);
    let mut reader = gix_packetline::StreamingPeekableIter::new(
        &mut cursor,
        &[gix_packetline::PacketLineRef::Flush],
        false,
    );

    let mut refs = Vec::new();
    loop {
        let line = match reader.read_line() {
            Some(Ok(Ok(line))) => line,
            Some(Ok(Err(decode))) => return Err(PktLineError::Decode(decode)),
            Some(Err(io)) => return Err(PktLineError::Io(io)),
            None => break,
        };
        let line_bytes: &[u8] = match line {
            gix_packetline::PacketLineRef::Data(b) => b,
            // flush/delimiter/end terminate the cmd list
            _ => break,
        };
        let line_str = std::str::from_utf8(line_bytes)
            .map_err(|e| PktLineError::MalformedCommand(format!("non-utf8 cmd: {e}")))?
            .trim_end_matches('\n');
        // Capabilities — first cmd has them after a NUL byte.
        let cmd_part = line_str.split('\0').next().unwrap_or(line_str);
        let parts: Vec<&str> = cmd_part.splitn(3, ' ').collect();
        if parts.len() != 3 {
            return Err(PktLineError::MalformedCommand(line_str.to_string()));
        }
        let upd = RefUpdate {
            old_oid: parts[0].to_string(),
            new_oid: parts[1].to_string(),
            ref_name: parts[2].to_string(),
        };
        debug!(?upd, "parsed receive-pack command");
        refs.push(upd);
    }
    if refs.is_empty() {
        return Err(PktLineError::NoCommands);
    }
    Ok(refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a pkt-line frame for `payload` (no trailing newline added).
    fn pkt(payload: &[u8]) -> Vec<u8> {
        let len = payload.len() + 4;
        let mut out = format!("{len:04x}").into_bytes();
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn parses_single_ref_update_with_capabilities() {
        let cmd = b"0000000000000000000000000000000000000000 deadbeefdeadbeefdeadbeefdeadbeefdeadbeef refs/heads/bot/foo\0report-status\n";
        let mut body = pkt(cmd);
        body.extend_from_slice(b"0000"); // flush
        let refs = parse_command_list(&body).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].ref_name, "refs/heads/bot/foo");
        assert_eq!(refs[0].old_oid, "0000000000000000000000000000000000000000");
        assert_eq!(refs[0].new_oid, "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
    }

    #[test]
    fn parses_multiple_ref_updates() {
        let mut body = Vec::new();
        body.extend(pkt(b"1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 refs/heads/bot/a\0report-status\n"));
        body.extend(pkt(b"3333333333333333333333333333333333333333 4444444444444444444444444444444444444444 refs/heads/bot/b\n"));
        body.extend_from_slice(b"0000");
        let refs = parse_command_list(&body).unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].ref_name, "refs/heads/bot/a");
        assert_eq!(refs[1].ref_name, "refs/heads/bot/b");
    }

    #[test]
    fn rejects_malformed_command() {
        let mut body = pkt(b"only-two parts\n");
        body.extend_from_slice(b"0000");
        assert!(matches!(
            parse_command_list(&body),
            Err(PktLineError::MalformedCommand(_))
        ));
    }

    #[test]
    fn rejects_empty_command_list() {
        let body = b"0000".to_vec();
        assert!(matches!(
            parse_command_list(&body),
            Err(PktLineError::NoCommands)
        ));
    }

    #[test]
    fn handles_force_push_marker_inline() {
        // Force-pushes look identical at the wire level — there's no
        // special marker, just `<old-oid> <new-oid> <ref>`. The new
        // oid being non-fast-forward is something the upstream resolves.
        let cmd = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb refs/heads/bot/force\0report-status\n";
        let mut body = pkt(cmd);
        body.extend_from_slice(b"0000");
        assert_eq!(parse_command_list(&body).unwrap().len(), 1);
    }
}
