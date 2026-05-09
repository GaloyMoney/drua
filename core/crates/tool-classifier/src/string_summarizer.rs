//! String → string compaction passes that operate on log-shaped text
//! while preserving the value's JSON type. Each pass mutates a
//! [`SegmentedText`] (the current text plus a line-segment map back to
//! the original) and substitutes runs of "boring" lines with XML-tag
//! marker blocks. The walker invokes the registered chain at every
//! `Value::String` leaf so the schema stays faithful — a string goes
//! in, a (typically smaller) string comes out.
//!
//! Design notes are in `core/notes/research-on-bash-output-handling.md`.
//! Highlights:
//!   - Markers are XML tags (`<nix-copy>...</nix-copy>`); body lines
//!     between the tags are the human-readable summary.
//!   - Each pass only operates on `Verbatim` segments — already-
//!     summarised regions are skipped structurally, not just by
//!     marker-text avoidance.
//!   - `replace_with_summary` is the single mutation primitive; it
//!     splits one Verbatim segment into [head][summary][tail] and
//!     shifts every downstream segment's `current` range by the
//!     line-count delta so subsequent passes can still reason about
//!     original line numbers.

use std::ops::Range;

/// Ordered, contiguous coverage of `SegmentedText::log` by line index.
/// Each segment knows which lines of the *current* string it spans
/// (`current`) and which lines of the *original* (pre-chain) string
/// they came from (`original`). `Summary` segments are synthetic —
/// produced by a pass — and `original` records the run that was
/// replaced.
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
}

/// One run of original text that a pass may inspect. Yielded by
/// [`SegmentedText::verbatim_regions`]; bodies sit on whole-line
/// boundaries so passes can do line-by-line scanning without
/// worrying about partial lines.
#[derive(Debug, Clone)]
pub struct VerbatimRegion<'a> {
    pub text: &'a str,
    /// Line index in the *current* `SegmentedText::log`.
    pub current: Range<u32>,
    /// Line index in the *original* (pre-chain) log.
    pub original: Range<u32>,
}

/// State carried through the [`StringSummarizerChain`]. Owns the
/// current text and the segment map back to the original.
#[derive(Debug, Clone)]
pub struct SegmentedText {
    log: String,
    segments: Vec<Segment>,
    original_lines: u32,
    original_bytes: u64,
    /// Byte offset of the start of each line in the *current* `log`.
    /// Maintained by every mutation; used to translate line ranges
    /// to byte ranges in O(1).
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

    /// Byte length of the slice covering `lines` (current
    /// coordinates). Useful for passes that need to populate
    /// `original-bytes` on a marker before calling
    /// [`replace_with_summary`].
    pub fn byte_len_of_lines(&self, lines: Range<u32>) -> u64 {
        let start = self.line_offsets[lines.start as usize];
        let end = self.line_offsets[lines.end as usize];
        (end - start) as u64
    }

    /// Translate a current line range to its original line range.
    /// Required by passes when emitting markers — the
    /// `original-lines="…"` attribute MUST refer to original input
    /// coordinates, not the current (post-prior-passes) coordinates.
    /// `current` must lie wholly inside one `Verbatim` segment;
    /// returns the identity range if no covering segment found
    /// (defensive — shouldn't happen in practice).
    pub fn current_to_original_range(&self, current: &Range<u32>) -> Range<u32> {
        if let Some(i) = self.find_verbatim_covering(current) {
            let seg = &self.segments[i];
            let start = seg.original.start + (current.start - seg.current.start);
            let end = seg.original.start + (current.end - seg.current.start);
            return start..end;
        }
        current.clone()
    }

    /// Translate a single current line index to original. Used by
    /// terminal passes (`BulkElide`) that touch ranges spanning
    /// multiple segments. For a current line inside a `Summary`
    /// segment, returns the start of that segment's original
    /// range — points within a synthetic marker don't map cleanly.
    pub fn current_to_original_line(&self, current_line: u32) -> u32 {
        for seg in &self.segments {
            if current_line >= seg.current.start && current_line < seg.current.end {
                return match seg.kind {
                    SegmentKind::Verbatim => {
                        seg.original.start + (current_line - seg.current.start)
                    }
                    SegmentKind::Summary { .. } => seg.original.start,
                };
            }
        }
        self.original_lines
    }

    /// Terminal compaction: keep the first `head_lines` AND the
    /// last `tail_lines` of the current log; replace the middle
    /// with a single `<bulk-elided>` marker, wrap head and tail in
    /// `<head>` / `<tail>` tags. Finer-grained segment mappings from
    /// earlier passes are flattened — this is the chain's last pass.
    ///
    /// Boundary snapping: if `head_lines` would split a `Summary`
    /// segment (cutting between e.g. `<nix-cache>` and
    /// `</nix-cache>`), we extend head past the segment's end so
    /// the marker survives intact. Symmetrical pull-back for the
    /// tail boundary. Already-summarised markers in head/tail
    /// regions are preserved verbatim.
    ///
    /// Returns `false` if `head_lines + tail_lines >= current_lines`
    /// or post-snap boundaries leave no middle to elide.
    pub fn elide_middle_keep_head_and_tail(
        &mut self,
        head_lines: u32,
        tail_lines: u32,
    ) -> bool {
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

        // Original-line coords for the marker. Translates head/tail
        // boundaries via the segment map so earlier-pass markers
        // surviving in head/tail still report correct absolute
        // input ranges.
        let head_original_end = self.current_to_original_line(middle_start_line);
        let tail_original_start = self.current_to_original_line(middle_end_line);

        let head_text = self.log[..middle_start_byte].to_string();
        let tail_text = self.log[middle_end_byte..].to_string();

        let middle_body = format!(
            "{} lines · {} bytes elided\n",
            elided_lines, elided_bytes,
        );
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

        // Segment map is approximate — bulk-elide is terminal so
        // downstream consumers only read `log`, not the map. We
        // emit one Verbatim segment covering the full new log
        // for inspector compatibility.
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

    /// Iterate over every `Verbatim` segment as a view into the
    /// current log. `Summary` segments are skipped — passes can't
    /// observe (and won't re-summarise) regions a previous pass
    /// already claimed.
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

    /// Replace `current_lines` (a line range that must lie wholly
    /// inside one `Verbatim` segment) with `new_text`. Splits the
    /// covering segment into `[head?][summary][tail?]`, shifts every
    /// downstream segment's `current` range by the line delta, and
    /// records `original_bytes`/`kept_bytes` on the summary segment
    /// for later accounting.
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

    /// If `line` falls inside a `Summary` segment, return the
    /// segment's `current.end` (one past the marker's last line);
    /// else return `line` unchanged. Used by `BulkElide` to push
    /// the head boundary past a straddling marker so the marker's
    /// open and close tags stay in the same kept region.
    fn snap_after_summary(&self, line: u32) -> u32 {
        for seg in &self.segments {
            if matches!(seg.kind, SegmentKind::Summary { .. })
                && line > seg.current.start
                && line < seg.current.end
            {
                return seg.current.end;
            }
        }
        line
    }

    /// If `line` falls inside a `Summary` segment, return the
    /// segment's `current.start`; else return `line` unchanged.
    /// Used by `BulkElide` to pull the tail boundary back so a
    /// marker straddling the boundary stays whole inside the tail.
    fn snap_before_summary(&self, line: u32) -> u32 {
        for seg in &self.segments {
            if matches!(seg.kind, SegmentKind::Summary { .. })
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
        // After replace_with_summary the segments before the splice
        // point are correct; from the splice onward the `current`
        // ranges need to be packed contiguously starting from the
        // first downstream segment. Walk the list and rebuild
        // current ranges to be contiguous, preserving each segment's
        // line span (computed from its current original-vs-current
        // size mapping, which doesn't change).
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

/// Render an opening tag with the standard accounting attributes
/// (what got dropped) plus any pass-specific extras. Body lines sit
/// between this and [`close_tag`]. Deliberately doesn't carry a
/// `kept-bytes` attribute — the agent reads the kept bytes directly
/// (they're literally the marker text); only the dropped count
/// needs an explicit number.
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

/// Build a complete marker block: open tag + body + close tag. Body
/// is whatever the pass wants to render between the tags.
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

fn escape_attr(v: &str) -> String {
    v.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub trait StringSummarizer: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn can_summarize(&self, ctx: &SegmentedText) -> bool;
    /// Mutate `ctx` in place. Returns `true` if any rewrite happened.
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

    /// Run every pass once, in registration order. Each pass may
    /// rewrite multiple regions in one `apply` call; running passes
    /// only once keeps the overall pipeline O(passes × content).
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

/// Terminal fallback: if the post-chain log is still over
/// `max_total_bytes`, keep the first `head_lines` AND the last
/// `tail_lines`, and replace the middle with a single
/// `<bulk-elided>` marker. Renders like:
///
/// ```text
/// <head>
/// {first head_lines verbatim — usually setup chatter, task
///  markers, early `<nix-copy>` summaries from prior passes}
/// </head>
/// <bulk-elided original-lines="N-M" original-bytes="…">
/// K lines · X bytes elided
/// </bulk-elided>
/// <tail>
/// {last tail_lines verbatim — completion / failure / status}
/// </tail>
/// ```
///
/// Deliberately dumb — no per-region scoring, no segment-kind
/// awareness. Head and tail are just absolute line ranges from the
/// post-chain log. If markers from earlier passes happen to be in
/// the head or tail, they survive verbatim; if they were in the
/// middle, they're elided along with everything else.
pub struct BulkElide {
    pub max_total_bytes: usize,
    pub head_lines: u32,
    pub tail_lines: u32,
}

impl Default for BulkElide {
    fn default() -> Self {
        Self {
            max_total_bytes: 16 * 1024,
            head_lines: 30,
            tail_lines: 100,
        }
    }
}

impl BulkElide {
    pub fn with_max_bytes(mut self, n: usize) -> Self {
        self.max_total_bytes = n;
        self
    }

    pub fn with_head_lines(mut self, n: u32) -> Self {
        self.head_lines = n;
        self
    }

    pub fn with_tail_lines(mut self, n: u32) -> Self {
        self.tail_lines = n;
        self
    }
}

impl StringSummarizer for BulkElide {
    fn name(&self) -> &'static str {
        "bulk-elide"
    }

    fn can_summarize(&self, ctx: &SegmentedText) -> bool {
        ctx.log().len() > self.max_total_bytes
    }

    fn apply(&self, ctx: &mut SegmentedText) -> bool {
        let total = ctx.current_lines();
        if total <= self.head_lines + self.tail_lines {
            return false;
        }
        let pre_bytes = ctx.log().len();
        let did_elide =
            ctx.elide_middle_keep_head_and_tail(self.head_lines, self.tail_lines);
        if did_elide {
            // Warn-level: BulkElide is the dumb fallback — when it
            // fires, the structured passes weren't specific enough
            // for this content. Surface the trigger so an operator
            // can audit the log shape and decide if a new
            // `StringSummarizer` for this pattern is worth adding.
            tracing::warn!(
                pass = "bulk-elide",
                pre_bytes,
                post_bytes = ctx.log().len(),
                head_lines = self.head_lines,
                tail_lines = self.tail_lines,
                max_total_bytes = self.max_total_bytes,
                "drua_tool_classifier.bulk_elide.fallback_fired",
            );
        }
        did_elide
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
            // Find a single contiguous run in any verbatim region.
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
        // Original ranges are preserved through the splice.
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
        // Now run a second pass that *would* match if it saw the
        // synthetic marker text — verbatim_regions skips it.
        let regions: Vec<_> = ctx.verbatim_regions().map(|r| r.text.to_string()).collect();
        assert_eq!(regions, vec!["a\nb\n".to_string(), "d\ne\n".to_string()]);
    }

    #[test]
    fn original_line_mapping_survives_two_passes() {
        let raw = "a\nb\nfoo\nfoo\nfoo\nd\ne\nbar\nbar\nf\n";
        let mut ctx = SegmentedText::from_initial(raw);
        // Pass 1: collapse foo run (current 2..5) into a 1-line marker.
        ctx.replace_with_summary(2..5, "<one>3</one>\n", "p1");
        // After pass 1, current looks like:
        //   line 0: a   (orig 0)
        //   line 1: b   (orig 1)
        //   line 2: <one>...   (orig 2..5, summary)
        //   line 3: d   (orig 5)
        //   line 4: e   (orig 6)
        //   line 5: bar (orig 7)
        //   line 6: bar (orig 8)
        //   line 7: f   (orig 9)
        // Pass 2: collapse bar run at current 5..7.
        ctx.replace_with_summary(5..7, "<two>2</two>\n", "p2");
        // Verify the bar segment maps back to original lines 7..9.
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
        assert!(
            !open.contains("kept-bytes"),
            "kept-bytes was deliberately dropped"
        );
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
        assert_eq!(ctx.byte_len_of_lines(1..2), 5); // "bbbb\n"
    }

    #[test]
    fn bulk_elide_keeps_head_and_tail_with_wrappers() {
        let mut raw = String::new();
        for i in 0..1000 {
            raw.push_str(&format!("line {i:04} of unstructured chatter\n"));
        }
        let mut ctx = SegmentedText::from_initial(&raw);
        BulkElide {
            max_total_bytes: 2_048,
            head_lines: 5,
            tail_lines: 20,
        }
        .apply(&mut ctx);
        let log = ctx.log();
        // Head wrapper present, with first 5 lines verbatim.
        assert!(log.starts_with("<head>\n"), "log starts: {log:.120}");
        assert!(log.contains("</head>\n"));
        assert!(log.contains("line 0000 of unstructured chatter"));
        assert!(log.contains("line 0004 of unstructured chatter"));
        // Middle elided.
        assert!(log.contains("<bulk-elided"));
        assert!(log.contains("</bulk-elided>"));
        assert!(log.contains("975 lines"));
        // Tail wrapper present, with last 20 lines verbatim.
        assert!(log.contains("<tail>\n"));
        assert!(log.contains("</tail>\n"));
        assert!(log.contains("line 0980 of unstructured chatter"));
        assert!(log.contains("line 0999 of unstructured chatter"));
        // Middle lines gone.
        assert!(!log.contains("line 0500 of unstructured chatter"));
    }

    #[test]
    fn bulk_elide_skips_when_total_under_head_plus_tail() {
        // Log is small (5 lines), under head_lines + tail_lines
        // combined — cannot meaningfully elide.
        let mut ctx = SegmentedText::from_initial("a\nb\nc\nd\ne\n");
        let pass = BulkElide {
            max_total_bytes: 4,
            head_lines: 30,
            tail_lines: 100,
        };
        assert!(pass.can_summarize(&ctx));
        assert!(!pass.apply(&mut ctx));
        assert_eq!(ctx.log(), "a\nb\nc\nd\ne\n");
    }

    #[test]
    fn bulk_elide_skips_when_under_byte_budget() {
        let mut ctx = SegmentedText::from_initial("a\nb\nc\n");
        let pass = BulkElide::default();
        assert!(!pass.can_summarize(&ctx));
        assert!(!pass.apply(&mut ctx));
    }

    #[test]
    fn elide_middle_does_not_split_marker_across_head_boundary() {
        // A multi-line marker straddles where head=5 would land.
        // Boundary-snapping pushes head_end to the marker's
        // current.end, so the open and close tags stay in the
        // same kept region. Without snapping, the open tag would
        // be in head and the close tag would be in the elided
        // middle — broken XML.
        let mut raw = String::new();
        raw.push_str("a\nb\nc\nd\nstraddle-1\nstraddle-2\nstraddle-3\nstraddle-4\n");
        for i in 0..200 {
            raw.push_str(&format!("middle line {i}\n"));
        }
        raw.push_str("tail-1\ntail-2\ntail-3\n");
        let mut ctx = SegmentedText::from_initial(&raw);
        // Replace lines 4..8 (current 0-indexed) with a 3-line
        // marker — straddles the head=5 boundary.
        ctx.replace_with_summary(4..8, "<sum>\nstraddle body\n</sum>\n", "p");
        ctx.elide_middle_keep_head_and_tail(5, 3);
        let log = ctx.log();
        assert!(log.contains("<sum>"));
        assert!(log.contains("straddle body"));
        assert!(log.contains("</sum>"));
        // Head wrapper closes after the full marker, not in the
        // middle of it.
        let head_end = log.find("</head>\n").expect("head closes");
        let head_section = &log[..head_end];
        assert!(head_section.contains("<sum>"));
        assert!(head_section.contains("</sum>"));
    }

    #[test]
    fn elide_middle_preserves_marker_in_head_when_in_first_n_lines() {
        // A marker emitted by an earlier pass that lands in the
        // head region survives verbatim inside <head>...</head>.
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
