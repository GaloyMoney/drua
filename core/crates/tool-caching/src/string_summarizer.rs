//! Log-text compaction: passes replace runs of boring lines with XML-tag markers.

use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Segment {
    pub current: Range<u32>,
    pub original: Range<u32>,
    pub kind: SegmentKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SegmentKind {
    Verbatim,
    Summary {
        by: &'static str,
        kept_bytes: u32,
        original_bytes: u64,
    },
    /// Lines injected by a preprocessor (e.g. wrapper tags). Render in
    /// the output, are skipped by `verbatim_regions()`, contribute zero
    /// original lines.
    Synthetic,
}

#[derive(Debug, Clone)]
pub struct VerbatimRegion<'a> {
    pub text: &'a str,
    pub current: Range<u32>,
    pub original: Range<u32>,
}

#[derive(Debug, Clone)]
pub struct SegmentedText {
    log: String,
    segments: Vec<Segment>,
    original_lines: u32,
    original_bytes: u64,
    line_offsets: Vec<usize>,
}

impl SegmentedText {
    pub fn from_initial(s: &str) -> Self {
        let line_offsets = compute_line_offsets(s);
        let lines = line_offsets.len().saturating_sub(1) as u32;
        let segments = if lines == 0 {
            Vec::new()
        } else {
            vec![Segment {
                current: 0..lines,
                original: 0..lines,
                kind: SegmentKind::Verbatim,
            }]
        };
        Self {
            log: s.to_string(),
            segments,
            original_lines: lines,
            original_bytes: s.len() as u64,
            line_offsets,
        }
    }

    pub fn log(&self) -> &str {
        &self.log
    }

    pub fn into_log(self) -> String {
        self.log
    }

    pub fn original_lines(&self) -> u32 {
        self.original_lines
    }

    pub fn original_bytes(&self) -> u64 {
        self.original_bytes
    }

    pub fn current_lines(&self) -> u32 {
        self.line_offsets.len().saturating_sub(1) as u32
    }

    pub fn current_bytes(&self) -> u32 {
        self.log.len().min(u32::MAX as usize) as u32
    }

    pub fn byte_len_of_lines(&self, lines: Range<u32>) -> u64 {
        let start = self.line_offsets[lines.start as usize];
        let end = self.line_offsets[lines.end as usize];
        (end - start) as u64
    }

    pub fn current_to_original_range(&self, current: &Range<u32>) -> Range<u32> {
        if let Some(i) = self.find_verbatim_covering(current) {
            let seg = &self.segments[i];
            let start = seg.original.start + (current.start - seg.current.start);
            let end = seg.original.start + (current.end - seg.current.start);
            return start..end;
        }
        current.clone()
    }

    pub fn current_to_original_line(&self, current_line: u32) -> u32 {
        for seg in &self.segments {
            if current_line >= seg.current.start && current_line < seg.current.end {
                return match seg.kind {
                    SegmentKind::Verbatim => {
                        seg.original.start + (current_line - seg.current.start)
                    }
                    SegmentKind::Summary { .. } | SegmentKind::Synthetic => seg.original.start,
                };
            }
        }
        self.original_lines
    }

    pub fn elide_middle_keep_head_and_tail(&mut self, head_lines: u32, tail_lines: u32) -> bool {
        let total = self.current_lines();
        if head_lines + tail_lines >= total {
            return false;
        }
        let middle_start_line = self.snap_after_summary(head_lines);
        let middle_end_line = self.snap_before_summary(total - tail_lines);
        if middle_start_line >= middle_end_line {
            return false;
        }
        let middle_start_byte = self.line_offsets[middle_start_line as usize];
        let middle_end_byte = self.line_offsets[middle_end_line as usize];
        let elided_bytes = (middle_end_byte - middle_start_byte) as u64;
        let elided_lines = middle_end_line - middle_start_line;

        let head_original_end = self.current_to_original_line(middle_start_line);
        let tail_original_start = self.current_to_original_line(middle_end_line);

        let head_text = self.log[..middle_start_byte].to_string();
        let tail_text = self.log[middle_end_byte..].to_string();

        let middle_body = format!("{} lines · {} bytes elided\n", elided_lines, elided_bytes,);
        let middle_marker = build_marker(
            "bulk-elided",
            head_original_end..tail_original_start,
            elided_bytes,
            &middle_body,
            &[],
        );

        let mut new_log = String::with_capacity(self.log.len());
        new_log.push_str("<head>\n");
        new_log.push_str(&head_text);
        if !head_text.ends_with('\n') {
            new_log.push('\n');
        }
        new_log.push_str("</head>\n");
        new_log.push_str(&middle_marker);
        new_log.push_str("<tail>\n");
        new_log.push_str(&tail_text);
        if !tail_text.ends_with('\n') {
            new_log.push('\n');
        }
        new_log.push_str("</tail>\n");

        self.log = new_log;
        self.line_offsets = compute_line_offsets(&self.log);

        let total_new = self.current_lines();
        self.segments = vec![Segment {
            current: 0..total_new,
            original: 0..self.original_lines,
            kind: SegmentKind::Verbatim,
        }];
        true
    }

    pub fn was_modified(&self) -> bool {
        self.segments
            .iter()
            .any(|s| matches!(s.kind, SegmentKind::Summary { .. }))
    }

    pub fn verbatim_regions(&self) -> impl Iterator<Item = VerbatimRegion<'_>> {
        self.segments
            .iter()
            .filter(|s| matches!(s.kind, SegmentKind::Verbatim))
            .map(|s| VerbatimRegion {
                text: self.slice_for_lines(s.current.clone()),
                current: s.current.clone(),
                original: s.original.clone(),
            })
    }

    /// Replace `current_lines` with `new_text`; range must lie within one `Verbatim` segment.
    pub fn replace_with_summary(
        &mut self,
        current_lines: Range<u32>,
        new_text: &str,
        by: &'static str,
    ) {
        assert!(
            current_lines.start < current_lines.end,
            "replace_with_summary: empty range",
        );
        let seg_idx = self
            .find_verbatim_covering(&current_lines)
            .expect("replace_with_summary: range must lie within one Verbatim segment");
        let seg = self.segments[seg_idx].clone();
        let SegmentKind::Verbatim = seg.kind else {
            unreachable!("find_verbatim_covering only returns Verbatim segments");
        };

        let byte_start = self.line_offsets[current_lines.start as usize];
        let byte_end = self.line_offsets[current_lines.end as usize];
        let original_bytes = (byte_end - byte_start) as u64;

        let new_text_lines = count_lines(new_text);
        debug_assert!(
            new_text.is_empty() || new_text.ends_with('\n'),
            "replace_with_summary: new_text must end with '\\n' (block markers are line-aligned)",
        );

        self.log.replace_range(byte_start..byte_end, new_text);

        let kept_bytes = new_text.len().min(u32::MAX as usize) as u32;

        let head_current = seg.current.start..current_lines.start;
        let summary_current_start = current_lines.start;
        let summary_current_end = summary_current_start + new_text_lines;
        let summary_current = summary_current_start..summary_current_end;

        let tail_current_orig_start = seg.original.start + (current_lines.end - seg.current.start);
        let tail_current_orig_end = seg.original.end;
        let tail_current_start = summary_current_end;
        let tail_current_end = tail_current_start + (seg.current.end - current_lines.end);
        let tail_current = tail_current_start..tail_current_end;

        let summary_original_start = seg.original.start + (current_lines.start - seg.current.start);
        let summary_original_end = seg.original.start + (current_lines.end - seg.current.start);
        let summary_original = summary_original_start..summary_original_end;

        let mut replacement = Vec::with_capacity(3);
        if head_current.start < head_current.end {
            replacement.push(Segment {
                current: head_current,
                original: seg.original.start..summary_original_start,
                kind: SegmentKind::Verbatim,
            });
        }
        replacement.push(Segment {
            current: summary_current,
            original: summary_original,
            kind: SegmentKind::Summary {
                by,
                kept_bytes,
                original_bytes,
            },
        });
        if tail_current.start < tail_current.end {
            replacement.push(Segment {
                current: tail_current,
                original: tail_current_orig_start..tail_current_orig_end,
                kind: SegmentKind::Verbatim,
            });
        }

        self.segments.splice(seg_idx..=seg_idx, replacement);
        self.recompute_line_offsets();
        self.normalise_segments_after_splice();
    }

    /// Wrap a contiguous range of `Verbatim` lines with `Synthetic` open
    /// and close tag lines, applying `line_transform` to each line in the
    /// range. The wrapped run stays walkable (`Verbatim`) so subsequent
    /// chain passes can still match its content; the synthetic tags
    /// render in the output but are skipped by `verbatim_regions()` and
    /// don't shift original line numbering.
    ///
    /// `before` and `after` must each end with `\n` and contribute one
    /// synthetic line each. `line_transform` must preserve line count
    /// (one input line → one output line, optionally with different
    /// content). Range must lie within a single `Verbatim` segment.
    pub fn wrap_verbatim_lines<F>(
        &mut self,
        current_lines: Range<u32>,
        before: &str,
        after: &str,
        line_transform: F,
    ) where
        F: Fn(&str) -> String,
    {
        assert!(
            current_lines.start < current_lines.end,
            "wrap_verbatim_lines: empty range",
        );
        assert!(
            before.ends_with('\n'),
            "wrap_verbatim_lines: `before` must end with '\\n'",
        );
        assert!(
            after.ends_with('\n'),
            "wrap_verbatim_lines: `after` must end with '\\n'",
        );

        let seg_idx = self
            .find_verbatim_covering(&current_lines)
            .expect("wrap_verbatim_lines: range must lie within one Verbatim segment");
        let seg = self.segments[seg_idx].clone();

        let byte_start = self.line_offsets[current_lines.start as usize];
        let byte_end = self.line_offsets[current_lines.end as usize];

        let inner_text = &self.log[byte_start..byte_end];
        let mut transformed = String::with_capacity(inner_text.len());
        let mut transformed_lines: u32 = 0;
        for raw_line in split_keep_newlines(inner_text) {
            let (body, newline) = match raw_line.strip_suffix('\n') {
                Some(b) => (b, "\n"),
                None => (raw_line, ""),
            };
            transformed.push_str(&line_transform(body));
            transformed.push_str(newline);
            transformed_lines += 1;
        }
        assert_eq!(
            transformed_lines,
            current_lines.end - current_lines.start,
            "wrap_verbatim_lines: line_transform must preserve line count",
        );

        let mut replacement_text =
            String::with_capacity(before.len() + transformed.len() + after.len());
        replacement_text.push_str(before);
        replacement_text.push_str(&transformed);
        replacement_text.push_str(after);

        self.log
            .replace_range(byte_start..byte_end, &replacement_text);

        let synthetic_before_lines = count_lines(before);
        let synthetic_after_lines = count_lines(after);
        let inner_lines = transformed_lines;

        let inner_original_start = seg.original.start + (current_lines.start - seg.current.start);
        let inner_original_end = seg.original.start + (current_lines.end - seg.current.start);

        let head_current = seg.current.start..current_lines.start;
        let synthetic_before_start = current_lines.start;
        let synthetic_before_end = synthetic_before_start + synthetic_before_lines;
        let inner_start = synthetic_before_end;
        let inner_end = inner_start + inner_lines;
        let synthetic_after_start = inner_end;
        let synthetic_after_end = synthetic_after_start + synthetic_after_lines;
        let tail_current_start = synthetic_after_end;
        let tail_current_end = tail_current_start + (seg.current.end - current_lines.end);

        let mut replacement = Vec::with_capacity(5);
        if head_current.start < head_current.end {
            replacement.push(Segment {
                current: head_current,
                original: seg.original.start..inner_original_start,
                kind: SegmentKind::Verbatim,
            });
        }
        replacement.push(Segment {
            current: synthetic_before_start..synthetic_before_end,
            original: inner_original_start..inner_original_start,
            kind: SegmentKind::Synthetic,
        });
        replacement.push(Segment {
            current: inner_start..inner_end,
            original: inner_original_start..inner_original_end,
            kind: SegmentKind::Verbatim,
        });
        replacement.push(Segment {
            current: synthetic_after_start..synthetic_after_end,
            original: inner_original_end..inner_original_end,
            kind: SegmentKind::Synthetic,
        });
        if tail_current_start < tail_current_end {
            replacement.push(Segment {
                current: tail_current_start..tail_current_end,
                original: inner_original_end..seg.original.end,
                kind: SegmentKind::Verbatim,
            });
        }

        self.segments.splice(seg_idx..=seg_idx, replacement);
        self.recompute_line_offsets();
        self.normalise_segments_after_splice();
    }

    fn snap_after_summary(&self, line: u32) -> u32 {
        for seg in &self.segments {
            if !matches!(seg.kind, SegmentKind::Verbatim)
                && line > seg.current.start
                && line < seg.current.end
            {
                return seg.current.end;
            }
        }
        line
    }

    fn snap_before_summary(&self, line: u32) -> u32 {
        for seg in &self.segments {
            if !matches!(seg.kind, SegmentKind::Verbatim)
                && line > seg.current.start
                && line < seg.current.end
            {
                return seg.current.start;
            }
        }
        line
    }

    fn find_verbatim_covering(&self, current_lines: &Range<u32>) -> Option<usize> {
        for (i, seg) in self.segments.iter().enumerate() {
            if !matches!(seg.kind, SegmentKind::Verbatim) {
                continue;
            }
            if seg.current.start <= current_lines.start && current_lines.end <= seg.current.end {
                return Some(i);
            }
        }
        None
    }

    fn slice_for_lines(&self, lines: Range<u32>) -> &str {
        let start = self.line_offsets[lines.start as usize];
        let end = self.line_offsets[lines.end as usize];
        &self.log[start..end]
    }

    fn recompute_line_offsets(&mut self) {
        self.line_offsets = compute_line_offsets(&self.log);
    }

    fn normalise_segments_after_splice(&mut self) {
        let mut cursor: u32 = 0;
        for s in self.segments.iter_mut() {
            let span = s.current.end - s.current.start;
            s.current = cursor..cursor + span;
            cursor += span;
        }
    }
}

fn compute_line_offsets(s: &str) -> Vec<usize> {
    if s.is_empty() {
        return vec![0];
    }
    let mut offsets = Vec::with_capacity(s.matches('\n').count() + 2);
    offsets.push(0);
    for (i, b) in s.bytes().enumerate() {
        if b == b'\n' {
            offsets.push(i + 1);
        }
    }
    if !s.ends_with('\n') {
        offsets.push(s.len());
    }
    offsets
}

fn count_lines(s: &str) -> u32 {
    if s.is_empty() {
        return 0;
    }
    let trailing = if s.ends_with('\n') { 0 } else { 1 };
    (s.matches('\n').count() + trailing) as u32
}

fn split_keep_newlines(s: &str) -> impl Iterator<Item = &str> {
    let mut start = 0usize;
    let bytes = s.as_bytes();
    std::iter::from_fn(move || {
        if start >= bytes.len() {
            return None;
        }
        let mut i = start;
        while i < bytes.len() && bytes[i] != b'\n' {
            i += 1;
        }
        let end = if i < bytes.len() { i + 1 } else { i };
        let slice = &s[start..end];
        start = end;
        Some(slice)
    })
}

pub fn open_tag(
    name: &'static str,
    original_lines: Range<u32>,
    original_bytes: u64,
    extra: &[(&str, &str)],
) -> String {
    use std::fmt::Write;
    let mut s = format!(
        "<{name} original-lines=\"{}-{}\" original-bytes=\"{}\"",
        original_lines.start + 1,
        original_lines.end,
        original_bytes,
    );
    for (k, v) in extra {
        let _ = write!(s, " {k}=\"{}\"", escape_attr(v));
    }
    s.push_str(">\n");
    s
}

pub fn close_tag(name: &'static str) -> String {
    format!("</{name}>\n")
}

pub fn build_marker(
    tag: &'static str,
    original_lines: Range<u32>,
    original_bytes: u64,
    body: &str,
    extra: &[(&str, &str)],
) -> String {
    let open = open_tag(tag, original_lines, original_bytes, extra);
    let close = close_tag(tag);
    format!("{open}{body}{close}")
}

pub fn escape_attr(v: &str) -> String {
    v.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn collect_runs<F: Fn(&str) -> bool>(ctx: &SegmentedText, pred: F) -> Vec<Range<u32>> {
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

pub fn apply_runs<F: Fn(u32, u64) -> String>(
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

pub trait StringSummarizer: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn can_summarize(&self, ctx: &SegmentedText) -> bool;
    fn apply(&self, ctx: &mut SegmentedText) -> bool;
}

pub struct StringSummarizerChain {
    passes: Vec<Box<dyn StringSummarizer>>,
}

impl StringSummarizerChain {
    pub fn new() -> Self {
        Self { passes: Vec::new() }
    }

    pub fn register(mut self, pass: impl StringSummarizer) -> Self {
        self.passes.push(Box::new(pass));
        self
    }

    pub fn run(&self, ctx: &mut SegmentedText) -> bool {
        let mut any = false;
        for pass in &self.passes {
            if pass.can_summarize(ctx) && pass.apply(ctx) {
                any = true;
            }
        }
        any
    }
}

impl Default for StringSummarizerChain {
    fn default() -> Self {
        Self::new()
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    struct PrefixSummarizer {
        name: &'static str,
        prefix: &'static str,
        marker_tag: &'static str,
    }

    impl StringSummarizer for PrefixSummarizer {
        fn name(&self) -> &'static str {
            self.name
        }
        fn can_summarize(&self, ctx: &SegmentedText) -> bool {
            ctx.verbatim_regions()
                .any(|r| r.text.lines().any(|l| l.starts_with(self.prefix)))
        }
        fn apply(&self, ctx: &mut SegmentedText) -> bool {
            let runs: Vec<(Range<u32>, Range<u32>)> = ctx
                .verbatim_regions()
                .flat_map(|r| {
                    let mut out = Vec::new();
                    let mut start: Option<u32> = None;
                    let lines: Vec<&str> = r.text.lines().collect();
                    for (i, line) in lines.iter().enumerate() {
                        let abs = r.current.start + i as u32;
                        let is_match = line.starts_with(self.prefix);
                        if is_match && start.is_none() {
                            start = Some(abs);
                        } else if !is_match {
                            if let Some(s) = start.take() {
                                out.push((s..abs, r.original.clone()));
                            }
                        }
                    }
                    if let Some(s) = start.take() {
                        out.push((s..r.current.end, r.original.clone()));
                    }
                    out
                })
                .collect();
            if runs.is_empty() {
                return false;
            }
            for (run, _orig) in runs.into_iter().rev() {
                let original_lines_count = run.end - run.start;
                let body_open = open_tag(self.marker_tag, 0..original_lines_count, 0, &[]);
                let body = format!("{} matched\n", original_lines_count);
                let close = close_tag(self.marker_tag);
                let marker = format!("{body_open}{body}{close}");
                ctx.replace_with_summary(run, &marker, self.name);
            }
            true
        }
    }

    #[test]
    fn from_initial_yields_one_verbatim_segment() {
        let ctx = SegmentedText::from_initial("a\nb\nc\n");
        assert_eq!(ctx.original_lines(), 3);
        let regions: Vec<_> = ctx.verbatim_regions().collect();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].text, "a\nb\nc\n");
        assert_eq!(regions[0].current, 0..3);
        assert_eq!(regions[0].original, 0..3);
    }

    #[test]
    fn replace_in_middle_splits_into_three_segments() {
        let mut ctx = SegmentedText::from_initial("a\nb\nc\nd\ne\n");
        ctx.replace_with_summary(1..3, "X\n", "test");
        let segs: Vec<_> = ctx.segments.clone();
        assert_eq!(segs.len(), 3);
        assert!(matches!(segs[0].kind, SegmentKind::Verbatim));
        assert!(matches!(segs[1].kind, SegmentKind::Summary { .. }));
        assert!(matches!(segs[2].kind, SegmentKind::Verbatim));
        assert_eq!(ctx.log(), "a\nX\nd\ne\n");
        assert_eq!(segs[0].original, 0..1);
        assert_eq!(segs[1].original, 1..3);
        assert_eq!(segs[2].original, 3..5);
    }

    #[test]
    fn replace_at_start_keeps_only_summary_and_tail() {
        let mut ctx = SegmentedText::from_initial("a\nb\nc\n");
        ctx.replace_with_summary(0..2, "X\n", "test");
        assert_eq!(ctx.segments.len(), 2);
        assert!(matches!(ctx.segments[0].kind, SegmentKind::Summary { .. }));
        assert!(matches!(ctx.segments[1].kind, SegmentKind::Verbatim));
        assert_eq!(ctx.log(), "X\nc\n");
        assert_eq!(ctx.segments[0].original, 0..2);
        assert_eq!(ctx.segments[1].original, 2..3);
    }

    #[test]
    fn replace_at_end_keeps_only_head_and_summary() {
        let mut ctx = SegmentedText::from_initial("a\nb\nc\n");
        ctx.replace_with_summary(1..3, "X\n", "test");
        assert_eq!(ctx.segments.len(), 2);
        assert!(matches!(ctx.segments[0].kind, SegmentKind::Verbatim));
        assert!(matches!(ctx.segments[1].kind, SegmentKind::Summary { .. }));
        assert_eq!(ctx.log(), "a\nX\n");
    }

    #[test]
    fn replace_whole_log_yields_single_summary_segment() {
        let mut ctx = SegmentedText::from_initial("a\nb\nc\n");
        ctx.replace_with_summary(0..3, "X\n", "test");
        assert_eq!(ctx.segments.len(), 1);
        assert!(matches!(ctx.segments[0].kind, SegmentKind::Summary { .. }));
        assert_eq!(ctx.log(), "X\n");
    }

    #[test]
    fn second_pass_skips_summary_segments() {
        let mut ctx = SegmentedText::from_initial("a\nb\nfoo\nfoo\nfoo\nd\ne\n");
        ctx.replace_with_summary(2..5, "<test>3 matched</test>\n", "test1");
        let regions: Vec<_> = ctx.verbatim_regions().map(|r| r.text.to_string()).collect();
        assert_eq!(regions, vec!["a\nb\n".to_string(), "d\ne\n".to_string()]);
    }

    #[test]
    fn original_line_mapping_survives_two_passes() {
        let raw = "a\nb\nfoo\nfoo\nfoo\nd\ne\nbar\nbar\nf\n";
        let mut ctx = SegmentedText::from_initial(raw);
        ctx.replace_with_summary(2..5, "<one>3</one>\n", "p1");
        ctx.replace_with_summary(5..7, "<two>2</two>\n", "p2");
        let bar_seg = ctx
            .segments
            .iter()
            .find(|s| matches!(&s.kind, SegmentKind::Summary { by, .. } if *by == "p2"))
            .expect("p2 summary present");
        assert_eq!(bar_seg.original, 7..9);
    }

    #[test]
    fn chain_runs_two_passes_in_order() {
        let raw = "a\nfoo\nfoo\nb\nbar\nbar\nc\n";
        let mut ctx = SegmentedText::from_initial(raw);
        let chain = StringSummarizerChain::new()
            .register(PrefixSummarizer {
                name: "p_foo",
                prefix: "foo",
                marker_tag: "foo-sum",
            })
            .register(PrefixSummarizer {
                name: "p_bar",
                prefix: "bar",
                marker_tag: "bar-sum",
            });
        let modified = chain.run(&mut ctx);
        assert!(modified);
        let by: Vec<_> = ctx
            .segments
            .iter()
            .filter_map(|s| match &s.kind {
                SegmentKind::Summary { by, .. } => Some(*by),
                _ => None,
            })
            .collect();
        assert_eq!(by, vec!["p_foo", "p_bar"]);
    }

    #[test]
    fn open_close_tag_render_drop_metadata_only() {
        let open = open_tag("nix-copy", 11..22, 945, &[]);
        assert!(open.contains("<nix-copy"));
        assert!(open.contains("original-lines=\"12-22\""));
        assert!(open.contains("original-bytes=\"945\""));
        assert!(!open.contains("kept-bytes"));
        let close = close_tag("nix-copy");
        assert_eq!(close, "</nix-copy>\n");
    }

    #[test]
    fn was_modified_false_until_first_replace() {
        let mut ctx = SegmentedText::from_initial("a\nb\n");
        assert!(!ctx.was_modified());
        ctx.replace_with_summary(0..1, "X\n", "p");
        assert!(ctx.was_modified());
    }

    #[test]
    fn byte_len_of_lines_matches_slice() {
        let raw = "aaaa\nbbbb\ncccc\n";
        let ctx = SegmentedText::from_initial(raw);
        assert_eq!(ctx.byte_len_of_lines(0..3), 15);
        assert_eq!(ctx.byte_len_of_lines(1..2), 5);
    }

    #[test]
    fn elide_middle_does_not_split_marker_across_head_boundary() {
        let mut raw = String::new();
        raw.push_str("a\nb\nc\nd\nstraddle-1\nstraddle-2\nstraddle-3\nstraddle-4\n");
        for i in 0..200 {
            raw.push_str(&format!("middle line {i}\n"));
        }
        raw.push_str("tail-1\ntail-2\ntail-3\n");
        let mut ctx = SegmentedText::from_initial(&raw);
        ctx.replace_with_summary(4..8, "<sum>\nstraddle body\n</sum>\n", "p");
        ctx.elide_middle_keep_head_and_tail(5, 3);
        let log = ctx.log();
        assert!(log.contains("<sum>"));
        assert!(log.contains("straddle body"));
        assert!(log.contains("</sum>"));
        let head_end = log.find("</head>\n").expect("head closes");
        let head_section = &log[..head_end];
        assert!(head_section.contains("<sum>"));
        assert!(head_section.contains("</sum>"));
    }

    #[test]
    fn elide_middle_preserves_marker_in_head_when_in_first_n_lines() {
        let mut raw = String::new();
        raw.push_str("a\nb\nfoo\nfoo\nfoo\nd\ne\n");
        for i in 0..200 {
            raw.push_str(&format!("middle line {i}\n"));
        }
        raw.push_str("tail-1\ntail-2\ntail-3\n");
        let mut ctx = SegmentedText::from_initial(&raw);
        ctx.replace_with_summary(2..5, "<sum>3 foos</sum>\n", "p");
        ctx.elide_middle_keep_head_and_tail(5, 3);
        let log = ctx.log();
        assert!(log.starts_with("<head>"));
        assert!(log.contains("<sum>3 foos</sum>"));
        assert!(log.contains("<bulk-elided"));
        assert!(log.contains("<tail>"));
        assert!(log.contains("tail-1"));
        assert!(log.contains("tail-3"));
    }
}
