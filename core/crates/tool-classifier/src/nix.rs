//! Three [`StringSummarizer`] passes covering the dominant nix-build
//! noise sources. Each pass scans `LogContext::verbatim_regions` for
//! its target shape and substitutes contiguous matching runs with an
//! XML marker block.
//!
//! Order matters at registration time: `NixFailureBlock` is registered
//! BEFORE `NixDrvList` and `NixCopyRun` so the failure block (which
//! often contains `> /nix/store/...drv` lines that look superficially
//! like a drv list) gets claimed first.
//!
//! Markers emitted:
//!   - `<nix-copy>`     — `copying path '/nix/store/...' from ...` runs
//!   - `<nix-drv-list>` — `these N derivations will be built:` + path list
//!   - `<nix-failure>`  — `error: builder for ... failed` and the
//!     indented `> ` log_tail that follows. Tail lines are kept
//!     inline (the agent-actionable signal).

use regex::Regex;
use std::ops::Range;
use std::sync::OnceLock;

use crate::string_summarizer::{
    close_tag, open_tag, LogContext, StringSummarizer, VerbatimRegion,
};

pub struct NixCopyRun;
pub struct NixDrvList;
pub struct NixFailureBlock;

const COPYING_PREFIX: &str = "copying path '/nix/store/";
const DRV_LIST_HEADER_PREFIX: &str = "these ";
const DRV_LIST_HEADER_SUFFIX: &str = " derivations will be built:";
const STORE_PATH_PREFIX: &str = "/nix/store/";

impl StringSummarizer for NixCopyRun {
    fn name(&self) -> &'static str {
        "nix-copy"
    }

    fn can_summarize(&self, ctx: &LogContext) -> bool {
        ctx.verbatim_regions()
            .any(|r| r.text.lines().any(|l| l.starts_with(COPYING_PREFIX)))
    }

    fn apply(&self, ctx: &mut LogContext) -> bool {
        let runs = collect_runs(ctx, |line| line.starts_with(COPYING_PREFIX));
        apply_runs(ctx, runs, "nix-copy", |count, _bytes| {
            format!("{count} paths\n")
        })
    }
}

impl StringSummarizer for NixDrvList {
    fn name(&self) -> &'static str {
        "nix-drv-list"
    }

    fn can_summarize(&self, ctx: &LogContext) -> bool {
        ctx.verbatim_regions().any(|r| {
            r.text.lines().any(|l| {
                l.starts_with(DRV_LIST_HEADER_PREFIX) && l.contains(DRV_LIST_HEADER_SUFFIX)
            })
        })
    }

    fn apply(&self, ctx: &mut LogContext) -> bool {
        // A drv-list block is the header line plus the contiguous run
        // of `/nix/store/...drv` lines that immediately follows it.
        let runs: Vec<Range<u32>> = ctx
            .verbatim_regions()
            .flat_map(|r| collect_drv_list_runs(&r))
            .collect();
        apply_runs(ctx, runs, "nix-drv-list", |count, _bytes| {
            // count includes the header line; subtract for the
            // human-readable derivation count.
            let drvs = count.saturating_sub(1);
            format!("{drvs} derivations to build\n")
        })
    }
}

fn collect_drv_list_runs(r: &VerbatimRegion<'_>) -> Vec<Range<u32>> {
    let lines: Vec<&str> = r.text.lines().collect();
    let mut runs = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.starts_with(DRV_LIST_HEADER_PREFIX) && line.contains(DRV_LIST_HEADER_SUFFIX) {
            let header_abs = r.current.start + i as u32;
            let mut j = i + 1;
            while j < lines.len() && lines[j].starts_with(STORE_PATH_PREFIX) {
                j += 1;
            }
            // Only collapse if there's at least one drv line — a
            // bare header with nothing following isn't worth a marker.
            if j > i + 1 {
                let end_abs = r.current.start + j as u32;
                runs.push(header_abs..end_abs);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    runs
}

impl StringSummarizer for NixFailureBlock {
    fn name(&self) -> &'static str {
        "nix-failure"
    }

    fn can_summarize(&self, ctx: &LogContext) -> bool {
        let re = failure_header_re();
        ctx.verbatim_regions()
            .any(|r| r.text.lines().any(|l| re.is_match(l)))
    }

    fn apply(&self, ctx: &mut LogContext) -> bool {
        let re = failure_header_re();
        let mut work: Vec<(Range<u32>, FailureMeta)> = Vec::new();
        for region in ctx.verbatim_regions() {
            let lines: Vec<&str> = region.text.lines().collect();
            let mut i = 0;
            while i < lines.len() {
                if let Some(cap) = re.captures(lines[i]) {
                    let drv = cap
                        .get(1)
                        .or_else(|| cap.get(3))
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default();
                    let inline_reason = cap
                        .get(2)
                        .or_else(|| cap.get(4))
                        .map(|m| m.as_str().trim().to_string())
                        .filter(|s| !s.is_empty());
                    let mut reason = inline_reason;
                    let mut log_tail: Vec<String> = Vec::new();
                    let mut j = i + 1;
                    while j < lines.len() {
                        let next = lines[j];
                        let trimmed = next.trim_start();
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
                        if trimmed.is_empty()
                            || trimmed.starts_with("Last ")
                            || trimmed.starts_with("Output path")
                            || trimmed.starts_with("For full logs")
                            || trimmed.starts_with("nix log ")
                            || trimmed.starts_with(STORE_PATH_PREFIX)
                        {
                            j += 1;
                            continue;
                        }
                        break;
                    }
                    let header_abs = region.current.start + i as u32;
                    let end_abs = region.current.start + j as u32;
                    work.push((
                        header_abs..end_abs,
                        FailureMeta {
                            drv,
                            reason,
                            log_tail,
                        },
                    ));
                    i = j;
                } else {
                    i += 1;
                }
            }
        }
        if work.is_empty() {
            return false;
        }
        let original_bytes_per_run: Vec<u64> = work
            .iter()
            .map(|(r, _)| byte_len_of_lines(ctx, r.clone()))
            .collect();
        for ((run, meta), original_bytes) in work.into_iter().zip(original_bytes_per_run).rev() {
            let count = run.end - run.start;
            let mut body = String::new();
            for line in &meta.log_tail {
                body.push_str("> ");
                body.push_str(line);
                body.push('\n');
            }
            let mut extra: Vec<(&str, &str)> = Vec::new();
            if !meta.drv.is_empty() {
                extra.push(("drv", meta.drv.as_str()));
            }
            if let Some(r) = meta.reason.as_deref() {
                extra.push(("reason", r));
            }
            let open = open_tag(
                "nix-failure",
                run.start..run.start + count,
                original_bytes,
                0,
                &extra,
            );
            let close = close_tag("nix-failure");
            let marker = format!("{open}{body}{close}");
            ctx.replace_with_summary(run, &marker, "nix-failure");
        }
        true
    }
}

struct FailureMeta {
    drv: String,
    reason: Option<String>,
    log_tail: Vec<String>,
}

fn collect_runs<F: Fn(&str) -> bool>(ctx: &LogContext, pred: F) -> Vec<Range<u32>> {
    let mut runs = Vec::new();
    for region in ctx.verbatim_regions() {
        let lines: Vec<&str> = region.text.lines().collect();
        let mut start: Option<u32> = None;
        for (i, line) in lines.iter().enumerate() {
            let abs = region.current.start + i as u32;
            if pred(line) {
                if start.is_none() {
                    start = Some(abs);
                }
            } else if let Some(s) = start.take() {
                if abs - s >= 2 {
                    runs.push(s..abs);
                }
            }
        }
        if let Some(s) = start.take() {
            if region.current.end - s >= 2 {
                runs.push(s..region.current.end);
            }
        }
    }
    runs
}

fn apply_runs<F: Fn(u32, u64) -> String>(
    ctx: &mut LogContext,
    runs: Vec<Range<u32>>,
    tag: &'static str,
    body_for: F,
) -> bool {
    if runs.is_empty() {
        return false;
    }
    let bytes_per_run: Vec<u64> = runs
        .iter()
        .map(|r| byte_len_of_lines(ctx, r.clone()))
        .collect();
    for (run, original_bytes) in runs.into_iter().zip(bytes_per_run).rev() {
        let count = run.end - run.start;
        let body = body_for(count, original_bytes);
        let open = open_tag(tag, run.start..run.start + count, original_bytes, 0, &[]);
        let close = close_tag(tag);
        let marker = format!("{open}{body}{close}");
        ctx.replace_with_summary(run, &marker, tag);
    }
    true
}

fn byte_len_of_lines(ctx: &LogContext, lines: Range<u32>) -> u64 {
    let log = ctx.log();
    let line_iter = log.split_inclusive('\n').collect::<Vec<_>>();
    let mut bytes: u64 = 0;
    for i in lines.start as usize..lines.end as usize {
        if let Some(l) = line_iter.get(i) {
            bytes += l.len() as u64;
        }
    }
    bytes
}

fn failure_header_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"^error: (?:builder for '(/nix/store/[^']+\.drv)' failed(.*)|failed to build attribute '[^']+',\s*build of '(/nix/store/[^']+\.drv)(?:\^\*)?' failed:?(.*))$",
        )
        .expect("failure-header regex")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::string_summarizer::StringSummarizerChain;

    fn run_chain(raw: &str) -> LogContext {
        let mut ctx = LogContext::from_initial(raw);
        let chain = StringSummarizerChain::new()
            .register(NixFailureBlock)
            .register(NixDrvList)
            .register(NixCopyRun);
        chain.run(&mut ctx);
        ctx
    }

    #[test]
    fn copy_run_collapses_contiguous_lines() {
        let raw = "header\n\
                   copying path '/nix/store/aaa-foo' from 'cache'\n\
                   copying path '/nix/store/bbb-bar' from 'cache'\n\
                   copying path '/nix/store/ccc-baz' from 'cache'\n\
                   tail\n";
        let ctx = run_chain(raw);
        assert!(ctx.log().contains("<nix-copy"));
        assert!(ctx.log().contains("3 paths"));
        assert!(ctx.log().contains("</nix-copy>"));
        assert!(ctx.log().contains("header"));
        assert!(ctx.log().contains("tail"));
        assert!(!ctx.log().contains("copying path"));
    }

    #[test]
    fn lone_copy_line_is_left_alone() {
        let raw = "header\ncopying path '/nix/store/aaa-foo' from 'cache'\ntail\n";
        let ctx = run_chain(raw);
        assert!(!ctx.log().contains("<nix-copy"));
        assert!(ctx.log().contains("copying path"));
    }

    #[test]
    fn drv_list_collapses_header_plus_paths() {
        let raw = "before\n\
                   these 3 derivations will be built:\n\
                   /nix/store/aaa-foo.drv\n\
                   /nix/store/bbb-bar.drv\n\
                   /nix/store/ccc-baz.drv\n\
                   after\n";
        let ctx = run_chain(raw);
        assert!(ctx.log().contains("<nix-drv-list"));
        assert!(ctx.log().contains("3 derivations to build"));
        assert!(ctx.log().contains("</nix-drv-list>"));
        assert!(!ctx.log().contains("/nix/store/aaa-foo.drv"));
    }

    #[test]
    fn failure_block_keeps_log_tail_inline() {
        let raw = "\
building '/nix/store/aaa-clippy.drv'
error: builder for '/nix/store/aaa-clippy.drv' failed with exit code 101; last 25 log lines:
       > error[E0063]: missing fields `auth_mode` and `internal_only`
       >   --> core/tests/toolset.rs:15:29
       >    |
       > 15 |     let cfg = McpUpstreamConfig {
       >    |                             ^^^^^^^^^^^^^^^^^
some unrelated trailing line
";
        let ctx = run_chain(raw);
        let log = ctx.log();
        assert!(log.contains("<nix-failure"));
        assert!(log.contains("drv=\"/nix/store/aaa-clippy.drv\""));
        assert!(log.contains("E0063"));
        assert!(log.contains("McpUpstreamConfig"));
        assert!(log.contains("</nix-failure>"));
    }

    #[test]
    fn failure_handles_flake_check_shape_with_reason_continuation() {
        let raw = "\
error: failed to build attribute 'checks.x86_64-linux.clippy', build of '/nix/store/aaa-clippy.drv^*' failed:
       Reason: builder failed with exit code 101.
       > error[E0277]: trait bound not satisfied
       > some other line
unrelated trailing
";
        let ctx = run_chain(raw);
        let log = ctx.log();
        assert!(log.contains("drv=\"/nix/store/aaa-clippy.drv\""));
        assert!(log.contains("reason="));
        assert!(log.contains("E0277"));
    }

    #[test]
    fn passes_compose_on_a_realistic_log() {
        let raw = "\
=== with-nix-cache: setup done ===
+ cd repo
+ nix run .#bats
copying path '/nix/store/aaa-source' from 'file:///nix-cache'
copying path '/nix/store/bbb-source' from 'file:///nix-cache'
copying path '/nix/store/ccc-source' from 'file:///nix-cache'
these 3 derivations will be built:
/nix/store/ddd-foo.drv
/nix/store/eee-bar.drv
/nix/store/fff-baz.drv
building '/nix/store/ddd-foo.drv'
error: builder for '/nix/store/ddd-foo.drv' failed with exit code 1; last 5 log lines:
       > error: oops
some final line
";
        let ctx = run_chain(raw);
        let log = ctx.log();
        // All three markers present
        assert!(log.contains("<nix-copy"), "log: {log}");
        assert!(log.contains("<nix-drv-list"), "log: {log}");
        assert!(log.contains("<nix-failure"), "log: {log}");
        // Original framing preserved
        assert!(log.contains("=== with-nix-cache: setup done ==="));
        assert!(log.contains("+ cd repo"));
        assert!(log.contains("some final line"));
    }
}
