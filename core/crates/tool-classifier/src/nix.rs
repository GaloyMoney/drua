//! Nix log summarizer passes.

use std::ops::Range;

use crate::string_summarizer::{
    apply_runs, collect_runs, escape_attr, SegmentedText, StringSummarizer, VerbatimRegion,
};

pub struct NixCopyRun;
pub struct NixDrvList;
pub struct NixFetchList;
pub struct NixBuildingRun;
pub struct NixCacheActivity;

const COPYING_PREFIX: &str = "copying path '/nix/store/";
const DRV_LIST_HEADER_SUFFIX: &str = "derivations will be built:";
const FETCH_LIST_HEADER_INFIX: &str = "paths will be fetched";
const STORE_PATH_PREFIX: &str = "/nix/store/";
const BUILDING_PREFIX: &str = "building '/nix/store/";
const NIX_CACHE_PREFIX: &str = "nix-cache: ";

fn is_drv_list_header(line: &str) -> bool {
    let t = line.trim_end();
    t.contains("these ") && t.ends_with(DRV_LIST_HEADER_SUFFIX)
}

fn is_fetch_list_header(line: &str) -> bool {
    let t = line.trim_end();
    t.contains("these ") && t.contains(FETCH_LIST_HEADER_INFIX) && t.ends_with(':')
}

impl StringSummarizer for NixCopyRun {
    fn name(&self) -> &'static str {
        "nix-copy"
    }
    fn can_summarize(&self, ctx: &SegmentedText) -> bool {
        ctx.verbatim_regions()
            .any(|r| r.text.lines().any(|l| l.starts_with(COPYING_PREFIX)))
    }
    fn apply(&self, ctx: &mut SegmentedText) -> bool {
        let runs = collect_runs(ctx, |line| line.starts_with(COPYING_PREFIX));
        apply_runs(ctx, runs, "nix-copy", |count, _bytes| {
            format!("{count} paths\n")
        })
    }
}

impl StringSummarizer for NixBuildingRun {
    fn name(&self) -> &'static str {
        "nix-building"
    }
    fn can_summarize(&self, ctx: &SegmentedText) -> bool {
        ctx.verbatim_regions()
            .any(|r| r.text.lines().any(|l| l.starts_with(BUILDING_PREFIX)))
    }
    fn apply(&self, ctx: &mut SegmentedText) -> bool {
        let runs = collect_runs(ctx, |line| line.starts_with(BUILDING_PREFIX));
        apply_runs(ctx, runs, "nix-building", |count, _bytes| {
            format!("{count} derivations built\n")
        })
    }
}

impl StringSummarizer for NixCacheActivity {
    fn name(&self) -> &'static str {
        "nix-cache"
    }
    fn can_summarize(&self, ctx: &SegmentedText) -> bool {
        ctx.verbatim_regions()
            .any(|r| r.text.lines().any(|l| l.starts_with(NIX_CACHE_PREFIX)))
    }
    fn apply(&self, ctx: &mut SegmentedText) -> bool {
        let runs = collect_runs(ctx, |line| line.starts_with(NIX_CACHE_PREFIX));
        apply_runs(ctx, runs, "nix-cache", |count, _bytes| {
            format!("{count} cache activity lines\n")
        })
    }
}

impl StringSummarizer for NixDrvList {
    fn name(&self) -> &'static str {
        "nix-drv-list"
    }
    fn can_summarize(&self, ctx: &SegmentedText) -> bool {
        ctx.verbatim_regions()
            .any(|r| r.text.lines().any(is_drv_list_header))
    }
    fn apply(&self, ctx: &mut SegmentedText) -> bool {
        let runs: Vec<Range<u32>> = ctx
            .verbatim_regions()
            .flat_map(|r| collect_list_block(&r, is_drv_list_header, |body| body.ends_with(".drv")))
            .collect();
        apply_runs(ctx, runs, "nix-drv-list", |count, _bytes| {
            let drvs = count.saturating_sub(1);
            format!("{drvs} derivations to build\n")
        })
    }
}

impl StringSummarizer for NixFetchList {
    fn name(&self) -> &'static str {
        "nix-fetch-list"
    }
    fn can_summarize(&self, ctx: &SegmentedText) -> bool {
        ctx.verbatim_regions()
            .any(|r| r.text.lines().any(is_fetch_list_header))
    }
    fn apply(&self, ctx: &mut SegmentedText) -> bool {
        let runs: Vec<Range<u32>> = ctx
            .verbatim_regions()
            .flat_map(|r| collect_list_block(&r, is_fetch_list_header, |_| true))
            .collect();
        apply_runs(ctx, runs, "nix-fetch-list", |count, _bytes| {
            let paths = count.saturating_sub(1);
            format!("{paths} paths to fetch\n")
        })
    }
}

fn collect_list_block<HF, BF>(r: &VerbatimRegion<'_>, is_header: HF, body_ok: BF) -> Vec<Range<u32>>
where
    HF: Fn(&str) -> bool,
    BF: Fn(&str) -> bool,
{
    let lines: Vec<&str> = r.text.lines().collect();
    let mut runs = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if is_header(lines[i]) {
            let header_abs = r.current.start + i as u32;
            let mut j = i + 1;
            while j < lines.len() {
                let body_trimmed = lines[j].trim_start();
                if body_trimmed.starts_with(STORE_PATH_PREFIX) && body_ok(body_trimmed) {
                    j += 1;
                } else {
                    break;
                }
            }
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

pub struct NixDerivationPreprocessor;

fn derivation_prefix(line: &str) -> Option<&str> {
    let body = line.strip_suffix('\n').unwrap_or(line);
    let idx = body.find("> ")?;
    let prefix = &body[..idx];
    if prefix.is_empty() {
        return None;
    }
    if !prefix
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return None;
    }
    Some(prefix)
}

fn strip_derivation_prefix<'a>(line: &'a str, prefix: &str) -> &'a str {
    let head = line
        .strip_prefix(prefix)
        .and_then(|r| r.strip_prefix("> "))
        .unwrap_or(line);
    head
}

impl StringSummarizer for NixDerivationPreprocessor {
    fn name(&self) -> &'static str {
        "nix-derivation"
    }

    fn can_summarize(&self, ctx: &SegmentedText) -> bool {
        ctx.verbatim_regions()
            .any(|r| r.text.lines().any(|l| derivation_prefix(l).is_some()))
    }

    fn apply(&self, ctx: &mut SegmentedText) -> bool {
        let mut runs: Vec<(Range<u32>, String)> = Vec::new();
        for region in ctx.verbatim_regions() {
            let lines: Vec<&str> = region.text.lines().collect();
            let mut i = 0usize;
            while i < lines.len() {
                let prefix = match derivation_prefix(lines[i]) {
                    Some(p) => p.to_string(),
                    None => {
                        i += 1;
                        continue;
                    }
                };
                let mut j = i + 1;
                while j < lines.len() {
                    match derivation_prefix(lines[j]) {
                        Some(p) if p == prefix => j += 1,
                        _ => break,
                    }
                }
                let start_abs = region.current.start + i as u32;
                let end_abs = region.current.start + j as u32;
                runs.push((start_abs..end_abs, prefix));
                i = j;
            }
        }
        if runs.is_empty() {
            return false;
        }
        let count = runs.len();
        for (range, prefix) in runs.into_iter().rev() {
            let before = format!("<nix-derivation name=\"{}\">\n", escape_attr(&prefix));
            let after = String::from("</nix-derivation>\n");
            let prefix_clone = prefix.clone();
            ctx.wrap_verbatim_lines(range, &before, &after, move |line| {
                strip_derivation_prefix(line, &prefix_clone).to_string()
            });
        }
        tracing::debug!(
            pass = "nix-derivation",
            wrapped = count,
            "drua_tool_classifier.string_summarizer.fired",
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::string_summarizer::StringSummarizerChain;

    fn run_chain(raw: &str) -> SegmentedText {
        let mut ctx = SegmentedText::from_initial(raw);
        let chain = StringSummarizerChain::new()
            .register(NixDrvList)
            .register(NixFetchList)
            .register(NixCopyRun)
            .register(NixBuildingRun)
            .register(NixCacheActivity);
        chain.run(&mut ctx);
        ctx
    }

    #[test]
    fn derivation_prefix_recognises_typical_names() {
        assert_eq!(derivation_prefix("drua> X\n"), Some("drua"));
        assert_eq!(
            derivation_prefix("drua-conf.json> X\n"),
            Some("drua-conf.json")
        );
        assert_eq!(derivation_prefix("drua.tar.gz> X"), Some("drua.tar.gz"));
        assert_eq!(derivation_prefix("plain line\n"), None);
        assert_eq!(derivation_prefix(">> not a prefix\n"), None);
        assert_eq!(derivation_prefix("> empty prefix\n"), None);
    }

    #[test]
    fn nix_derivation_preprocessor_wraps_prefix_runs() {
        let raw = "these 1 derivations will be built:\n\
                   building '/nix/store/aaa-drua.drv'...\n\
                   drua> Running phase: buildPhase\n\
                   drua>    Compiling foo v0.1.0\n\
                   drua>    Compiling bar v0.1.0\n\
                   drua>    Finished `release` in 2m 09s\n";
        let mut ctx = SegmentedText::from_initial(raw);
        let chain = StringSummarizerChain::new()
            .register(NixDerivationPreprocessor)
            .register(crate::cargo::CargoCompileRun);
        chain.run(&mut ctx);
        let log = ctx.log();
        assert!(log.contains("<nix-derivation name=\"drua\">"));
        assert!(log.contains("</nix-derivation>"));
        assert!(log.contains("Running phase: buildPhase"));
        assert!(log.contains("Finished `release` in 2m 09s"));
        assert!(log.contains("<cargo-compiling"));
        assert!(log.contains("2 crates compiled"));
        assert!(!log.contains("drua> "));
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
    fn drv_list_collapses_indented_paths() {
        let raw = "nix build plan: these 3 derivations will be built:\n\
                     /nix/store/8fikg-cargo-src-antlr4rust.drv\n\
                     /nix/store/02n23-cargo-package.drv\n\
                     /nix/store/9z5s0-dummyBuild.drv\n\
                   after\n";
        let ctx = run_chain(raw);
        let log = ctx.log();
        assert!(log.contains("<nix-drv-list"));
        assert!(log.contains("3 derivations to build"));
        assert!(!log.contains("antlr4rust"));
    }

    #[test]
    fn fetch_list_collapses_indented_store_paths() {
        let raw = "before\n\
                   these 4 paths will be fetched (3.8 GiB download, 5.5 GiB unpacked):\n\
                     /nix/store/1xqvw-Cargo.toml\n\
                     /nix/store/4g8yb-Cargo.toml\n\
                     /nix/store/5flrm-Cargo.toml\n\
                     /nix/store/6dn78-Cargo.toml\n\
                   after\n";
        let ctx = run_chain(raw);
        let log = ctx.log();
        assert!(log.contains("<nix-fetch-list"));
        assert!(log.contains("4 paths to fetch"));
        assert!(!log.contains("Cargo.toml"));
        assert!(log.contains("before"));
        assert!(log.contains("after"));
    }

    #[test]
    fn building_run_collapses_contiguous_lines() {
        let raw = "header\n\
                   building '/nix/store/aaa-foo.drv'...\n\
                   building '/nix/store/bbb-bar.drv'...\n\
                   building '/nix/store/ccc-baz.drv'...\n\
                   building '/nix/store/ddd-qux.drv'...\n\
                   tail\n";
        let ctx = run_chain(raw);
        let log = ctx.log();
        assert!(log.contains("<nix-building"));
        assert!(log.contains("4 derivations built"));
        assert!(!log.contains("aaa-foo.drv"));
        assert!(log.contains("header"));
        assert!(log.contains("tail"));
    }

    #[test]
    fn cache_activity_collapses_run_including_status_lines() {
        let raw = "before\n\
                   nix-cache: saving new store paths to local cache...\n\
                   nix-cache: calculating cache size...\n\
                   nix-cache: cache size is 28438 MB\n\
                   nix-cache: over limit, pruning to ~25 GB...\n\
                   nix-cache: removing /nix-cache/nar/aaa.nar\n\
                   nix-cache: removing /nix-cache/nar/bbb.nar\n\
                   nix-cache: deleted 50 files so far, recalculating size...\n\
                   nix-cache: cache size is 26639 MB\n\
                   nix-cache: pruning done, removed 250 files\n\
                   nix-cache: done saving to local cache\n\
                   after\n";
        let ctx = run_chain(raw);
        let log = ctx.log();
        assert!(log.contains("<nix-cache"));
        assert!(log.contains("</nix-cache>"));
        assert!(log.contains("before"));
        assert!(log.contains("after"));
        assert_eq!(log.matches("nix-cache: ").count(), 0);
    }

    #[test]
    fn lone_nix_cache_line_is_left_alone() {
        let raw = "=== with-nix-cache: start ===\n\
                   nix-cache: local cache found (25G), restoring\n\
                   === with-nix-cache: setup done ===\n";
        let ctx = run_chain(raw);
        let log = ctx.log();
        assert!(log.contains("nix-cache: local cache found"));
        assert!(!log.contains("<nix-cache"));
    }

    #[test]
    fn passes_compose_on_realistic_log() {
        let raw = "\
+ cd repo
+ nix run .#bats
copying path '/nix/store/aaa-source' from 'file:///nix-cache'
copying path '/nix/store/bbb-source' from 'file:///nix-cache'
copying path '/nix/store/ccc-source' from 'file:///nix-cache'
nix build plan: these 3 derivations will be built:
  /nix/store/ddd-foo.drv
  /nix/store/eee-bar.drv
  /nix/store/fff-baz.drv
these 4 paths will be fetched (1 GiB download, 2 GiB unpacked):
  /nix/store/path1-name
  /nix/store/path2-name
  /nix/store/path3-name
  /nix/store/path4-name
building '/nix/store/ddd-foo.drv'...
building '/nix/store/eee-bar.drv'...
building '/nix/store/fff-baz.drv'...
nix-cache: saving...
nix-cache: cache size is 100 MB
nix-cache: done saving
final
";
        let ctx = run_chain(raw);
        let log = ctx.log();
        assert!(log.contains("<nix-copy"));
        assert!(log.contains("<nix-drv-list"));
        assert!(log.contains("<nix-fetch-list"));
        assert!(log.contains("<nix-building"));
        assert!(log.contains("<nix-cache"));
        assert!(log.contains("+ cd repo"));
        assert!(log.contains("final"));
    }

    /// Regression: marker `original-lines` must reference input
    /// coords even after earlier passes shifted current line numbers.
    #[test]
    fn original_lines_attribute_refers_to_input_coords_across_passes() {
        let mut raw = String::new();
        for i in 0..50 {
            raw.push_str(&format!(
                "copying path '/nix/store/p{i:03}-thing' from 'cache'\n"
            ));
        }
        for i in 0..8 {
            raw.push_str(&format!("spacer line {i}\n"));
        }
        for i in 0..12 {
            raw.push_str(&format!("building '/nix/store/b{i:02}-foo.drv'...\n"));
        }
        let ctx = run_chain(&raw);
        let log = ctx.log();
        let buildings: Vec<&str> = log
            .lines()
            .filter(|l| l.starts_with("<nix-building"))
            .collect();
        assert_eq!(buildings.len(), 1);
        let attr = buildings[0]
            .split_whitespace()
            .find(|s| s.starts_with("original-lines="))
            .expect("original-lines present");
        assert_eq!(attr, "original-lines=\"59-70\"");
    }

    #[test]
    fn marker_only_carries_drop_metadata_not_kept_bytes() {
        let raw = "header\n\
                   copying path '/nix/store/aaa-foo' from 'cache'\n\
                   copying path '/nix/store/bbb-bar' from 'cache'\n\
                   tail\n";
        let ctx = run_chain(raw);
        let log = ctx.log();
        assert!(log.contains("original-lines="));
        assert!(log.contains("original-bytes="));
        assert!(!log.contains("kept-bytes="));
    }
}
