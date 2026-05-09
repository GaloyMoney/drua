//! `git clone` stderr chatter collapser.

use std::ops::Range;

use crate::string_summarizer::{build_marker, SegmentedText, StringSummarizer};

pub struct GitCloneProgress;

impl StringSummarizer for GitCloneProgress {
    fn name(&self) -> &'static str {
        "git-clone"
    }

    fn can_summarize(&self, ctx: &SegmentedText) -> bool {
        ctx.verbatim_regions()
            .any(|r| r.text.lines().any(is_git_progress))
    }

    fn apply(&self, ctx: &mut SegmentedText) -> bool {
        let mut runs: Vec<Range<u32>> = Vec::new();
        for region in ctx.verbatim_regions() {
            let lines: Vec<&str> = region.text.lines().collect();
            let mut start: Option<u32> = None;
            for (i, line) in lines.iter().enumerate() {
                let abs = region.current.start + i as u32;
                if is_git_progress(line) {
                    if start.is_none() {
                        start = Some(abs);
                    }
                } else if let Some(s) = start.take() {
                    if abs - s >= 3 {
                        runs.push(s..abs);
                    }
                }
            }
            if let Some(s) = start.take() {
                if region.current.end - s >= 3 {
                    runs.push(s..region.current.end);
                }
            }
        }
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
            let body = format!("{count} progress lines\n");
            let marker = build_marker("git-clone", original_range, original_bytes, &body, &[]);
            ctx.replace_with_summary(run, &marker, "git-clone");
        }
        tracing::debug!(
            pass = "git-clone",
            run_count,
            lines_collapsed = total_lines,
            bytes_collapsed = total_bytes,
            "drua_tool_classifier.string_summarizer.fired",
        );
        true
    }
}

fn is_git_progress(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("Cloning into '")
        || trimmed.starts_with("remote: ")
        || trimmed.starts_with("Receiving objects:")
        || trimmed.starts_with("Resolving deltas:")
        || trimmed.starts_with("Updating files:")
        || trimmed.starts_with("Filtering content:")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::string_summarizer::StringSummarizerChain;

    fn run_chain(raw: &str) -> SegmentedText {
        let mut ctx = SegmentedText::from_initial(raw);
        let chain = StringSummarizerChain::new().register(GitCloneProgress);
        chain.run(&mut ctx);
        ctx
    }

    #[test]
    fn collapses_remote_progress_run() {
        let raw = "before\n\
                   Cloning into '/tmp/build/get'...\n\
                   remote: Enumerating objects: 11550, done.\n\
                   remote: Counting objects:   0% (1/1910)\n\
                   remote: Counting objects:   1% (20/1910)\n\
                   remote: Counting objects:   2% (39/1910)\n\
                   remote: Counting objects: 100% (1910/1910), done.\n\
                   remote: Compressing objects:  10% (10/100)\n\
                   Receiving objects: 100% (11550/11550), done.\n\
                   Resolving deltas: 100% (4321/4321), done.\n\
                   after\n";
        let ctx = run_chain(raw);
        let log = ctx.log();
        assert!(log.contains("<git-clone"));
        assert!(log.contains("progress lines"));
        assert!(log.contains("</git-clone>"));
        assert!(log.contains("before"));
        assert!(log.contains("after"));
        assert!(!log.contains("Counting objects:"));
        assert!(!log.contains("Receiving objects:"));
    }

    #[test]
    fn lone_remote_line_is_left_alone() {
        let raw = "noise\nremote: just one line\nmore noise\n";
        let ctx = run_chain(raw);
        assert!(!ctx.log().contains("<git-clone"));
        assert!(ctx.log().contains("remote: just one line"));
    }
}
