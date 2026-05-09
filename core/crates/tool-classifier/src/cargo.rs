//! Cargo log summarizers. Diagnostic blocks (`warning:`, `error:`) are kept verbatim.

use std::ops::Range;

use crate::string_summarizer::{build_marker, SegmentedText, StringSummarizer};

pub struct CargoDownloadRun;
pub struct CargoCompileRun;
pub struct CargoCheckRun;

fn is_download_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("Downloading ") || t.starts_with("Downloaded ")
}

fn is_compile_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("Compiling ")
}

fn is_check_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("Checking ")
}

impl StringSummarizer for CargoDownloadRun {
    fn name(&self) -> &'static str {
        "cargo-downloads"
    }
    fn can_summarize(&self, ctx: &SegmentedText) -> bool {
        ctx.verbatim_regions()
            .any(|r| r.text.lines().any(is_download_line))
    }
    fn apply(&self, ctx: &mut SegmentedText) -> bool {
        let runs = collect_runs(ctx, is_download_line);
        apply_runs(ctx, runs, "cargo-downloads", |count, _| {
            format!("{count} crates downloaded\n")
        })
    }
}

impl StringSummarizer for CargoCompileRun {
    fn name(&self) -> &'static str {
        "cargo-compiling"
    }
    fn can_summarize(&self, ctx: &SegmentedText) -> bool {
        ctx.verbatim_regions()
            .any(|r| r.text.lines().any(is_compile_line))
    }
    fn apply(&self, ctx: &mut SegmentedText) -> bool {
        let runs = collect_runs(ctx, is_compile_line);
        apply_runs(ctx, runs, "cargo-compiling", |count, _| {
            format!("{count} crates compiled\n")
        })
    }
}

impl StringSummarizer for CargoCheckRun {
    fn name(&self) -> &'static str {
        "cargo-checking"
    }
    fn can_summarize(&self, ctx: &SegmentedText) -> bool {
        ctx.verbatim_regions()
            .any(|r| r.text.lines().any(is_check_line))
    }
    fn apply(&self, ctx: &mut SegmentedText) -> bool {
        let runs = collect_runs(ctx, is_check_line);
        apply_runs(ctx, runs, "cargo-checking", |count, _| {
            format!("{count} crates checked\n")
        })
    }
}

fn collect_runs<F: Fn(&str) -> bool>(ctx: &SegmentedText, pred: F) -> Vec<Range<u32>> {
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
    ctx: &mut SegmentedText,
    runs: Vec<Range<u32>>,
    tag: &'static str,
    body_for: F,
) -> bool {
    if runs.is_empty() {
        return false;
    }
    let run_count = runs.len();
    let metas: Vec<(Range<u32>, Range<u32>, u64)> = runs
        .into_iter()
        .map(|r| {
            let orig = ctx.current_to_original_range(&r);
            let bytes = ctx.byte_len_of_lines(r.clone());
            (r, orig, bytes)
        })
        .collect();
    let total_lines: u32 = metas.iter().map(|(r, _, _)| r.end - r.start).sum();
    let total_bytes: u64 = metas.iter().map(|(_, _, b)| *b).sum();
    for (run, original_range, original_bytes) in metas.into_iter().rev() {
        let count = run.end - run.start;
        let body = body_for(count, original_bytes);
        let marker = build_marker(tag, original_range, original_bytes, &body, &[]);
        ctx.replace_with_summary(run, &marker, tag);
    }
    tracing::debug!(
        pass = tag,
        run_count,
        lines_collapsed = total_lines,
        bytes_collapsed = total_bytes,
        "drua_tool_classifier.string_summarizer.fired",
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::string_summarizer::StringSummarizerChain;

    fn run(raw: &str) -> SegmentedText {
        let mut ctx = SegmentedText::from_initial(raw);
        let chain = StringSummarizerChain::new()
            .register(CargoDownloadRun)
            .register(CargoCompileRun)
            .register(CargoCheckRun);
        chain.run(&mut ctx);
        ctx
    }

    #[test]
    fn downloads_collapse() {
        let raw = "    Updating crates.io index\n\
                   Downloading crates ...\n\
                   Downloaded aead v0.5.2\n\
                   Downloaded anstyle v1.0.14\n\
                   Downloaded darling_macro v0.20.11\n\
                   Downloaded anyhow v1.0.102\n\
                   Compiling a v0.1\n";
        let ctx = run(raw);
        let log = ctx.log();
        assert!(log.contains("<cargo-downloads"));
        assert!(log.contains("crates downloaded"));
        assert!(!log.contains("Downloaded aead"));
        assert!(log.contains("Updating crates.io index"));
    }

    #[test]
    fn compiling_collapses() {
        let raw = "header\n\
                   Compiling a v0.1\n\
                   Compiling b v0.2\n\
                   Compiling c v0.3\n\
                   Compiling d v0.4\n\
                   Finished `dev` profile [unoptimized] target(s) in 3m 33s\n";
        let ctx = run(raw);
        let log = ctx.log();
        assert!(log.contains("<cargo-compiling"));
        assert!(log.contains("4 crates compiled"));
        assert!(log.contains("Finished `dev`"));
        assert!(!log.contains("Compiling a "));
    }

    #[test]
    fn checking_collapses() {
        let raw = "    Checking foo v0.1.0\n    Checking bar v0.2.0\n    Checking baz v0.3.0\n";
        let ctx = run(raw);
        assert!(ctx.log().contains("<cargo-checking"));
        assert!(ctx.log().contains("3 crates checked"));
    }

    #[test]
    fn warnings_and_errors_pass_through_verbatim() {
        let raw = "Compiling a v0.1\n\
                   Compiling b v0.2\n\
                   warning: unused variable: `x`\n   --> src/main.rs:42:9\n\
                   error[E0277]: the trait `Send` is not implemented\n   --> src/lib.rs:5:1\n\
                   error: could not compile `myapp` (lib) due to 1 previous error\n";
        let ctx = run(raw);
        let log = ctx.log();
        assert!(log.contains("warning: unused variable"));
        assert!(log.contains("error[E0277]"));
        assert!(log.contains("could not compile"));
    }

    #[test]
    fn lone_compile_line_left_alone() {
        let raw = "header\nCompiling foo v0.1\ntail\n";
        let ctx = run(raw);
        assert!(!ctx.log().contains("<cargo-compiling"));
        assert!(ctx.log().contains("Compiling foo v0.1"));
    }
}
