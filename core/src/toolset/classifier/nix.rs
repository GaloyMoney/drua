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
use super::{
    Classification, ClassifierContext, ClassifierError, ResultClassifier, ToolResultSummary,
};

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

pub struct NixBuildClassifier;

impl ResultClassifier for NixBuildClassifier {
    fn name(&self) -> &str {
        "nix::build::v1"
    }

    /// No identity-based match. Nix output reaches this classifier
    /// either via content-sniff (bash running nix) or via region
    /// recursion (`classify_region` called by a parent), neither of
    /// which carries a useful `tool_name`. See `matches_content`.
    fn matches(&self, _tool_name: &str, _args: &serde_json::Value) -> bool {
        false
    }

    fn matches_content(&self, ctx: &ClassifierContext<'_>) -> bool {
        let text = extract_text(ctx.raw);
        sniff(&text)
    }

    fn classify(&self, ctx: &ClassifierContext<'_>) -> Result<Classification, ClassifierError> {
        let raw = extract_text(ctx.raw);
        let summary = parse(&raw);
        Ok(Classification {
            summary: ToolResultSummary::NixBuild(summary),
            canonical_text: raw,
        })
    }
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
    let secondary = SECONDARY_RE.get_or_init(|| {
        Regex::new(
            r"(?m)^(\[\d{2}:\d{2}:\d{2}\]\s+)?(error: builder for '/nix/store/[^']+\.drv' failed|copying path '/nix/store/)",
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
    let failure_header = FAILURE_HEADER_RE.get_or_init(|| {
        Regex::new(
            r"(?m)^(\[\d{2}:\d{2}:\d{2}\]\s+)?error: builder for '(/nix/store/[^']+\.drv)' failed(.*)$",
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
            // Group 1 is the optional `[HH:MM:SS] ` prefix (concourse
            // wrapping); 2 is the drv path; 3 is the reason tail.
            let drv_path = cap.get(2).map(|m| m.as_str().to_string()).unwrap_or_default();
            let reason = cap
                .get(3)
                .map(|m| m.as_str().trim().to_string())
                .filter(|s| !s.is_empty());
            // Walk forward through `> <line>` continuation lines that
            // nix emits as its "Last N log lines" trailer. Concourse
            // wraps each line with a `[HH:MM:SS] ` prefix; strip that
            // first if present.
            let mut log_tail: Vec<String> = Vec::new();
            let mut j = i + 1;
            while j < lines.len() && log_tail.len() < MAX_FAILURE_LOG_TAIL {
                let next = lines[j];
                let unwrapped = strip_concourse_timestamp(next);
                if let Some(stripped) = unwrapped.strip_prefix("       > ") {
                    log_tail.push(stripped.to_string());
                } else if let Some(stripped) = unwrapped.strip_prefix("> ") {
                    log_tail.push(stripped.to_string());
                } else if unwrapped.trim().is_empty() {
                    j += 1;
                    continue;
                } else {
                    break;
                }
                j += 1;
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

fn extract_text(result: &rmcp::model::CallToolResult) -> String {
    if let Some(sc) = result.structured_content.as_ref() {
        if let Some(s) = sc.as_str() {
            return s.to_string();
        }
        if let Some(map) = sc.as_object() {
            if map.len() == 1 {
                if let Some(serde_json::Value::String(s)) = map.values().next() {
                    return s.clone();
                }
            }
        }
    }
    result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::Content;

    fn ctx_with(raw_text: &str) -> rmcp::model::CallToolResult {
        rmcp::model::CallToolResult::success(vec![Content::text(raw_text.to_string())])
    }

    fn no_recurse() -> impl Fn(&str) -> Option<ToolResultSummary> {
        |_| None
    }

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
    fn classifier_matches_content_when_nix_shape_present() {
        let raw = ctx_with(
            "building '/nix/store/aaaa-foo.drv'\ncopying path '/nix/store/bbbb-bar' from cache",
        );
        let no_args = serde_json::json!({});
        let no_rec = no_recurse();
        let ctx = ClassifierContext {
            tool_name: "bash",
            args: &no_args,
            raw: &raw,
            exit_code: None,
            classify_region: &no_rec,
        };
        assert!(NixBuildClassifier.matches_content(&ctx));
    }

    #[test]
    fn classifier_does_not_match_content_for_unrelated_bash_output() {
        let raw = ctx_with("ls /home/user\nfile1.txt\nfile2.txt");
        let no_args = serde_json::json!({});
        let no_rec = no_recurse();
        let ctx = ClassifierContext {
            tool_name: "bash",
            args: &no_args,
            raw: &raw,
            exit_code: None,
            classify_region: &no_rec,
        };
        assert!(!NixBuildClassifier.matches_content(&ctx));
    }
}
