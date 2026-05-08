//! `NixBuildClassifier` — typed summary for nix-build-shaped output.
//!
//! Fires in two contexts:
//!
//! 1. **Top-level content sniff.** Plain-text tools whose output looks
//!    like a nix build (bash running `nix build`, an MCP tool wrapping
//!    a nix invocation, …) land here via the registry's content-sniff
//!    pass — `matches_content` peeks at the bytes regardless of the
//!    syscall site's `tool_name`.
//! 2. **Region recursion from a parent classifier.** When
//!    `ConcourseLogsClassifier` extracts a failed-derivation log_tail,
//!    it calls `ctx.classify_region(&log_tail)`. If the tail itself
//!    contains nix-build output (a common shape when a derivation's
//!    builder recursively invokes nix, or when nix's own
//!    "Last N log lines" trailer carries another nix sequence), this
//!    classifier matches and produces a nested `NixBuild` summary
//!    that the parent stores in `NixBuildFailure.embedded`.
//!
//! What it extracts:
//!   - count of `^building '/nix/store/...drv'$` lines (derivations attempted)
//!   - count of `copying path '/nix/store/...' from '...'` lines (cache hits)
//!   - failures from `error: builder for '/nix/store/...' failed with ...` blocks
//!     each carrying the `> ` prefixed log_tail nix emits

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

use super::string_classifier::StringClassifier;

const MAX_FAILURES_KEPT: usize = 10;
const MAX_FAILURE_LOG_TAIL: usize = 40;

/// Each `error: builder for '/nix/store/xxx-foo.drv' failed ...`
/// block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NixDerivationFailure {
    /// `/nix/store/xxx-name.drv`.
    pub drv_path: String,
    /// e.g. "with exit code 101", "with exit code 1; last 25 log lines:".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Lines from nix's "Last N log lines:" trailer with the `> `
    /// prefix stripped — the actual builder diagnostic.
    pub log_tail: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NixBuildSummary {
    /// Count of `building '/nix/store/...drv'` lines.
    pub derivations_attempted: u32,
    /// Count of `copying path '/nix/store/...' from '...'` lines.
    pub cache_paths_copied: u32,
    /// Failed derivations with detail.
    pub failures: Vec<NixDerivationFailure>,
    pub total_bytes: u64,
    /// Approximate number of bytes the agent now sees vs the raw input.
    pub kept_bytes: u32,
}

/// `StringClassifier` impl — fires when the walker encounters a
/// nix-build-shaped string leaf (bash output that ran `nix build`,
/// concourse `logs` field after timestamp stripping, etc.). Returns
/// `Some(typed sentinel)` on match, letting the walker substitute the
/// raw string with a structured summary in `kept`.
pub struct NixStringClassifier;

impl StringClassifier for NixStringClassifier {
    fn name(&self) -> &str {
        "nix::string::v1"
    }

    fn classify(&self, text: &str) -> Option<serde_json::Value> {
        if !sniff(text) {
            return None;
        }
        let summary = parse(text);
        Some(serde_json::json!({
            "_typed": "nix_build",
            "summary": summary,
        }))
    }
}

/// Two co-occurring fingerprints: `building '/nix/store/...drv'` and
/// either `error: builder for '/nix/store/...' failed` OR
/// `copying path '/nix/store/...' from`. Single-line stragglers from
/// other tools (a bash session that happens to mention `/nix/store/`
/// in passing) won't trip both.
///
/// Patterns tolerate an optional `[HH:MM:SS]` timestamp prefix so the
/// classifier fires on concourse-wrapped nix output too — concourse's
/// per-task timestamping is the only structural difference between
/// "raw nix" and "nix wrapped by concourse".
fn sniff(text: &str) -> bool {
    static BUILDING_RE: OnceLock<Regex> = OnceLock::new();
    static SECONDARY_RE: OnceLock<Regex> = OnceLock::new();
    let building = BUILDING_RE.get_or_init(|| {
        Regex::new(r"(?m)^(\[\d{2}:\d{2}:\d{2}\]\s+)?building '/nix/store/[^']+\.drv'").unwrap()
    });
    // Three secondary fingerprints, any one of which (alongside or
    // independent of `building`) is enough to recognize nix output:
    //   - `error: builder for '/nix/store/...drv' failed`  (raw nix)
    //   - `error: failed to build attribute '...'`         (nix flake check)
    //   - `copying path '/nix/store/...' from`             (substituter)
    let secondary = SECONDARY_RE.get_or_init(|| {
        Regex::new(
            r"(?m)^(\[\d{2}:\d{2}:\d{2}\]\s+)?(error: builder for '/nix/store/[^']+\.drv' failed|error: failed to build attribute '|copying path '/nix/store/)",
        )
        .unwrap()
    });
    building.is_match(text) || secondary.is_match(text)
}

fn parse(raw: &str) -> NixBuildSummary {
    static BUILDING_RE: OnceLock<Regex> = OnceLock::new();
    static COPYING_RE: OnceLock<Regex> = OnceLock::new();
    static FAILURE_HEADER_RE: OnceLock<Regex> = OnceLock::new();

    let building = BUILDING_RE.get_or_init(|| {
        Regex::new(r"(?m)^(\[\d{2}:\d{2}:\d{2}\]\s+)?building '/nix/store/[^']+\.drv'").unwrap()
    });
    let copying = COPYING_RE.get_or_init(|| {
        Regex::new(r"(?m)^(\[\d{2}:\d{2}:\d{2}\]\s+)?copying path '/nix/store/[^']+' from").unwrap()
    });
    // Two failure-header shapes:
    //   A. `error: builder for '/nix/store/...drv' failed<REASON>`
    //      — raw nix-build output (one drv at a time).
    //   B. `error: failed to build attribute 'X', build of
    //          '/nix/store/...drv^*' failed: <REASON>`
    //      — `nix flake check` aggregating multiple drvs (the form
    //      concourse logs use). Reason can be inline OR on a
    //      subsequent indented `Reason: <…>` line.
    let failure_header = FAILURE_HEADER_RE.get_or_init(|| {
        Regex::new(
            r"(?m)^(\[\d{2}:\d{2}:\d{2}\]\s+)?error: (?:builder for '(/nix/store/[^']+\.drv)' failed(.*)|failed to build attribute '[^']+',\s*build of '(/nix/store/[^']+\.drv)(?:\^\*)?' failed:?(.*))$",
        )
        .unwrap()
    });

    let derivations_attempted = building.find_iter(raw).count() as u32;
    let cache_paths_copied = copying.find_iter(raw).count() as u32;

    let mut failures: Vec<NixDerivationFailure> = Vec::new();
    let lines: Vec<&str> = raw.lines().collect();
    let mut i = 0;
    while i < lines.len() && failures.len() < MAX_FAILURES_KEPT {
        let line = lines[i];
        if let Some(cap) = failure_header.captures(line) {
            // Capture groups (cap 1 = optional `[HH:MM:SS] ` prefix):
            //   shape A: cap 2 = drv,        cap 3 = inline reason
            //   shape B: cap 4 = drv (^*-stripped), cap 5 = inline reason
            let drv_path = cap
                .get(2)
                .or_else(|| cap.get(4))
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let mut reason = cap
                .get(3)
                .or_else(|| cap.get(5))
                .map(|m| m.as_str().trim().to_string())
                .filter(|s| !s.is_empty());
            // Walk forward through nix's "Last N log lines:" trailer
            // PLUS the optional `Reason:` continuation line (shape B).
            // Concourse wraps each line with a `[HH:MM:SS] ` prefix
            // and an indent — strip both before pattern-matching.
            let mut log_tail: Vec<String> = Vec::new();
            let mut j = i + 1;
            while j < lines.len() && log_tail.len() < MAX_FAILURE_LOG_TAIL {
                let next = lines[j];
                let unwrapped = strip_concourse_timestamp(next);
                let trimmed = unwrapped.trim_start();

                if let Some(rest) = trimmed.strip_prefix("Reason: ") {
                    if reason.is_none() {
                        reason = Some(rest.trim().to_string());
                    }
                    j += 1;
                    continue;
                }
                if let Some(stripped) = trimmed.strip_prefix("> ") {
                    log_tail.push(stripped.to_string());
                    j += 1;
                    continue;
                }
                // Skip non-content concourse/nix scaffolding so we
                // don't bail out before reaching the `> ` log lines.
                if trimmed.is_empty()
                    || trimmed.starts_with("Last ")
                    || trimmed.starts_with("Output path")
                    || trimmed.starts_with("For full logs")
                    || trimmed.starts_with("nix log ")
                    || trimmed.starts_with("/nix/store/")
                {
                    j += 1;
                    continue;
                }
                break;
            }
            failures.push(NixDerivationFailure {
                drv_path,
                reason,
                log_tail,
            });
            i = j;
            continue;
        }
        i += 1;
    }

    let total_bytes = raw.len() as u64;
    let kept_bytes = estimate_kept_bytes(&failures, derivations_attempted, cache_paths_copied);

    NixBuildSummary {
        derivations_attempted,
        cache_paths_copied,
        failures,
        total_bytes,
        kept_bytes,
    }
}

/// `[HH:MM:SS] foo` → `foo`. Lines without a timestamp prefix pass
/// through unchanged. Used so the line-walk for `> <line>` log_tail
/// continuation can match either raw nix output or concourse-wrapped
/// nix output.
fn strip_concourse_timestamp(line: &str) -> &str {
    if line.len() >= 11
        && line.starts_with('[')
        && line.as_bytes().get(3) == Some(&b':')
        && line.as_bytes().get(6) == Some(&b':')
        && line.as_bytes().get(9) == Some(&b']')
    {
        line[10..].strip_prefix(' ').unwrap_or(&line[10..])
    } else {
        line
    }
}

fn estimate_kept_bytes(
    failures: &[NixDerivationFailure],
    derivations: u32,
    cache_paths: u32,
) -> u32 {
    let mut bytes = 64u32; // header + counters
    for f in failures {
        bytes = bytes.saturating_add(f.drv_path.len() as u32 + 32);
        for line in &f.log_tail {
            bytes = bytes.saturating_add(line.len() as u32 + 4);
        }
    }
    // counter rendering ~= 64 bytes
    bytes = bytes
        .saturating_add(derivations.to_string().len() as u32)
        .saturating_add(cache_paths.to_string().len() as u32)
        .saturating_add(32);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_matches_building_drv_line() {
        let text = "preparing build\nbuilding '/nix/store/abcd-foo.drv'\nsome stdout";
        assert!(sniff(text));
    }

    #[test]
    fn sniff_matches_failure_header() {
        let text = "irrelevant noise\nerror: builder for '/nix/store/abcd-foo.drv' failed with exit code 1";
        assert!(sniff(text));
    }

    #[test]
    fn sniff_matches_copying_path() {
        let text = "copying path '/nix/store/abc-bar' from 'https://cache.nixos.org/'";
        assert!(sniff(text));
    }

    #[test]
    fn sniff_rejects_unrelated_bash_output() {
        let text = "ls /nix/store/abcd  # passing reference to nix store, not a build";
        assert!(!sniff(text));
    }

    #[test]
    fn sniff_matches_concourse_wrapped_nix() {
        // Concourse prefixes every line with `[HH:MM:SS] `; the Nix
        // classifier should fire either way so the same classifier
        // covers raw `nix build` output AND concourse-wrapped build
        // logs (delegated by the Concourse classifier or by the
        // walker descending into the `logs` field).
        let text = "[03:57:25] preparing to build\n\
                    [03:57:25] building '/nix/store/aaaa-foo.drv'\n\
                    [03:57:30] copying path '/nix/store/bbbb-bar' from 'https://cache.nixos.org/'";
        assert!(sniff(text));
    }

    #[test]
    fn parse_extracts_failure_under_concourse_timestamps() {
        let text = "\
[03:57:28] error: builder for '/nix/store/aaaa-clippy.drv' failed with exit code 101; last 25 log lines:
[03:57:28]        > error[E0063]: missing fields `auth_mode`
[03:57:28]        >   --> core/tests/toolset.rs:15:29
[03:57:29] some unrelated trailing line
";
        let s = parse(text);
        assert_eq!(s.failures.len(), 1);
        let f = &s.failures[0];
        assert_eq!(f.drv_path, "/nix/store/aaaa-clippy.drv");
        assert!(f.reason.as_deref().unwrap_or("").contains("exit code 101"));
        assert!(f.log_tail.iter().any(|l| l.contains("E0063")));
    }

    #[test]
    fn parse_counts_derivations_and_cache_copies() {
        let text = "\
building '/nix/store/aaaa-foo.drv'
building '/nix/store/bbbb-bar.drv'
copying path '/nix/store/cccc-baz' from 'https://cache.nixos.org/'
copying path '/nix/store/dddd-qux' from 'https://cache.nixos.org/'
copying path '/nix/store/eeee-quux' from 'https://cache.nixos.org/'
";
        let s = parse(text);
        assert_eq!(s.derivations_attempted, 2);
        assert_eq!(s.cache_paths_copied, 3);
        assert!(s.failures.is_empty());
    }

    #[test]
    fn parse_extracts_failure_with_log_tail() {
        let text = "\
building '/nix/store/aaaa-clippy.drv'
error: builder for '/nix/store/aaaa-clippy.drv' failed with exit code 101; last 25 log lines:
       > error[E0063]: missing fields `auth_mode` and `internal_only`
       >   --> core/tests/toolset.rs:15:29
       >    |
       > 15 |     let cfg = McpUpstreamConfig {
       >    |                             ^^^^^^^^^^^^^^^^^
some unrelated trailing line
";
        let s = parse(text);
        assert_eq!(s.failures.len(), 1);
        let f = &s.failures[0];
        assert_eq!(f.drv_path, "/nix/store/aaaa-clippy.drv");
        assert!(f.reason.as_deref().unwrap_or("").contains("exit code 101"));
        assert!(f.log_tail.iter().any(|l| l.contains("E0063")));
        assert!(f.log_tail.iter().any(|l| l.contains("toolset.rs:15:29")));
    }

    #[test]
    fn string_classifier_returns_typed_sentinel_on_nix_shape() {
        let text =
            "building '/nix/store/aaaa-foo.drv'\ncopying path '/nix/store/bbbb-bar' from cache";
        let v = NixStringClassifier
            .classify(text)
            .expect("nix shape should match");
        assert_eq!(v.get("_typed").and_then(|x| x.as_str()), Some("nix_build"));
        let summary = v.get("summary").expect("summary inline");
        assert_eq!(
            summary
                .get("derivations_attempted")
                .and_then(|x| x.as_u64()),
            Some(1)
        );
        assert_eq!(
            summary.get("cache_paths_copied").and_then(|x| x.as_u64()),
            Some(1)
        );
    }

    #[test]
    fn string_classifier_returns_none_on_unrelated_text() {
        let text = "ls /home/user\nfile1.txt\nfile2.txt";
        assert!(NixStringClassifier.classify(text).is_none());
    }
}
