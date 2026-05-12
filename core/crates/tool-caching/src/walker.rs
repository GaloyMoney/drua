use std::sync::Arc;

use serde_json::Value;

use crate::config::ToolCachingConfig;
use crate::preprocessors;
use crate::primitives::{ElidedPath, QueryStructure, ToolCallSummary, ToolInvocationId};
use crate::string_summarizer::{SegmentedText, StringSummarizerChain};

#[derive(Clone)]
pub struct Walker {
    chain: Arc<StringSummarizerChain>,
    threshold_bytes: usize,
    sentinel_min_bytes: usize,
    sentinel_hard_cap_bytes: usize,
}

impl Walker {
    pub fn new(chain: Arc<StringSummarizerChain>, config: &ToolCachingConfig) -> Self {
        Self {
            chain,
            threshold_bytes: config.generic_threshold_bytes,
            sentinel_min_bytes: config.sentinel_min_bytes,
            sentinel_hard_cap_bytes: config.sentinel_hard_cap_bytes,
        }
    }

    fn sentinel_budget(&self) -> usize {
        self.threshold_bytes
            .clamp(self.sentinel_min_bytes, self.sentinel_hard_cap_bytes)
    }

    pub fn summarize(
        &self,
        query_structure: &QueryStructure,
        invocation_id: ToolInvocationId,
        tool_name: &str,
    ) -> ToolCallSummary {
        let mut elided_paths = Vec::new();
        let root_path = "$";
        let original_bytes = json_size(&query_structure.root);
        let summary = self.walk(
            &query_structure.root,
            root_path,
            invocation_id,
            self.threshold_bytes,
            tool_name,
            &mut elided_paths,
        );
        ToolCallSummary {
            summary,
            elided_paths,
            root_path: root_path.to_string(),
            original_bytes,
        }
    }

    fn walk(
        &self,
        value: &Value,
        path: &str,
        invocation_id: ToolInvocationId,
        budget: usize,
        tool_name: &str,
        elided_paths: &mut Vec<ElidedPath>,
    ) -> Value {
        let size = json_size(value) as usize;
        // Floor: under this size, elision markers (~200 bytes) would bloat
        // the value rather than shrink it. Passthrough regardless of budget.
        if size < MIN_ELIDE_BYTES {
            return value.clone();
        }
        match value {
            // Strings ALWAYS run through summarize_string so the chain
            // gets a chance to compact boring runs (nix-copy, cargo-
            // compile, …) even when the string is sub-budget — the
            // savings on long log payloads are worth the cheap pattern
            // scans, and the chain has its own no-op fast paths.
            Value::String(s) => {
                self.summarize_string(s, path, invocation_id, budget, tool_name, elided_paths)
            }
            // Arrays / objects passthrough when sub-budget — their
            // children's chain passes can't help if the whole container
            // already fits.
            Value::Array(items) if size <= budget => Value::Array(items.clone()),
            Value::Object(map) if size <= budget => Value::Object(map.clone()),
            Value::Array(items) => {
                self.walk_array(items, path, invocation_id, budget, tool_name, elided_paths)
            }
            Value::Object(map) => {
                self.walk_object(map, path, invocation_id, budget, tool_name, elided_paths)
            }
            other => other.clone(),
        }
    }

    fn walk_array(
        &self,
        items: &[Value],
        path: &str,
        invocation_id: ToolInvocationId,
        budget: usize,
        tool_name: &str,
        elided_paths: &mut Vec<ElidedPath>,
    ) -> Value {
        let n = items.len();
        if n == 0 {
            return Value::Array(vec![]);
        }
        // Equal-share per item, minus container overhead (`[`, `]`, commas).
        let per_item = budget.saturating_sub(n + 2).max(1) / n;
        let paths_before = elided_paths.len();
        let original_bytes = json_size(&Value::Array(items.to_vec()));
        let walked: Vec<Value> = items
            .iter()
            .enumerate()
            .map(|(i, v)| {
                self.walk(
                    v,
                    &format!("{path}[{i}]"),
                    invocation_id,
                    per_item,
                    tool_name,
                    elided_paths,
                )
            })
            .collect();
        let walked_value = Value::Array(walked.clone());
        if (json_size(&walked_value) as usize) <= budget {
            return walked_value;
        }
        // 0/1-item arrays can't be meaningfully sentinel'd.
        if n <= 1 {
            return walked_value;
        }
        // Schema-conforming truncate: emit a shorter array (head ++ tail) of
        // the same element type so the wrapped outputSchema's `result` stays
        // valid. Truncation metadata moves into the ElidedPath, where the
        // structured envelope's `_elided.paths[i]` carries it. Per-item
        // elided_paths for kept items (e.g. $[0].body) survive; those for
        // dropped middle indices get pruned.
        self.truncate_array(
            &walked,
            n,
            original_bytes,
            path,
            invocation_id,
            paths_before,
            elided_paths,
        )
    }

    fn walk_object(
        &self,
        map: &serde_json::Map<String, Value>,
        path: &str,
        invocation_id: ToolInvocationId,
        budget: usize,
        tool_name: &str,
        elided_paths: &mut Vec<ElidedPath>,
    ) -> Value {
        // Per-key budget proportional to original byte share, so a fat
        // `body` field gets most of the budget and tiny ids get a sliver.
        let key_sizes: Vec<usize> = map.values().map(|v| json_size(v) as usize).collect();
        let total: usize = key_sizes.iter().sum::<usize>().max(1);
        let usable = budget.saturating_sub(map.len() + 2).max(1);
        let walked: serde_json::Map<String, Value> = map
            .iter()
            .zip(key_sizes.iter())
            .map(|((k, v), &size)| {
                let per_key = (usable.saturating_mul(size) / total).max(1);
                (
                    k.clone(),
                    self.walk(
                        v,
                        &format!("{path}.{k}"),
                        invocation_id,
                        per_key,
                        tool_name,
                        elided_paths,
                    ),
                )
            })
            .collect();
        Value::Object(walked)
    }

    fn summarize_string(
        &self,
        s: &str,
        path: &str,
        invocation_id: ToolInvocationId,
        budget: usize,
        tool_name: &str,
        elided_paths: &mut Vec<ElidedPath>,
    ) -> Value {
        // Tool-name preprocessor (concourse ANSI/timestamp strip, etc.)
        // runs first so downstream passes see clean text. Only meaningful
        // at the root — for nested strings we use the empty tool_name (no
        // preprocessor matches), since e.g. only the root concourse log
        // string benefits from ANSI stripping. The mapping flows back
        // from chain-compacted lines all the way to raw text via
        // `current_to_original_line` (compacted → preprocessed) plus
        // `preprocessed_to_raw` (preprocessed → raw).
        let preprocessed = if path == "$" {
            preprocessors::run(tool_name, s)
        } else {
            preprocessors::Preprocessed {
                text: s.to_string(),
                preprocessed_to_raw: preprocessors::identity_mapping(s),
            }
        };
        // Pattern passes (nix-copy, cargo-compile, rsync, …) compact runs
        // of boring lines in place. Operates on a SegmentedText so order
        // and line indices stay accurate across passes.
        let mut ctx = SegmentedText::from_initial(&preprocessed.text);
        let _ = self.chain.run(&mut ctx);
        let prepared = ctx.log().to_string();
        let modified = prepared != *s;
        // If the chain didn't change anything and we're already under
        // budget, passthrough verbatim.
        if !modified && s.len() <= budget {
            return Value::String(s.to_string());
        }
        // If the chain (or preprocessor) brought us under budget, we're
        // done — record a json_path recover (boundaries aren't
        // extractable from a chain-compacted text).
        if modified && prepared.len() <= budget {
            elided_paths.push(ElidedPath {
                path: path.to_string(),
                bytes: s.len() as u64,
                lines: Some(line_count(s)),
                length: None,
                head_count: None,
                tail_count: None,
                recover: make_full_recover(invocation_id, path),
            });
            return Value::String(prepared);
        }
        // Try line-mode first if the string has enough lines to split.
        if let Some(elide) =
            line_elide_string(&prepared, budget, &ctx, &preprocessed.preprocessed_to_raw)
        {
            elided_paths.push(ElidedPath {
                path: path.to_string(),
                bytes: s.len() as u64,
                lines: Some(line_count(s)),
                length: None,
                head_count: None,
                tail_count: None,
                recover: make_lines_recover(
                    invocation_id,
                    path,
                    elide.raw_offset as usize,
                    elide.raw_missing as usize,
                ),
            });
            return Value::String(elide.text);
        }
        // Fall back to byte-mode for single-line / few-line strings.
        // When preprocess / chain modified the string, the recovery
        // template can't carry byte offsets (they wouldn't map back to
        // the persisted raw bytes), so emit a full-value recover and
        // let the fetch cap gate response size.
        if modified {
            elided_paths.push(ElidedPath {
                path: path.to_string(),
                bytes: s.len() as u64,
                lines: None,
                length: None,
                head_count: None,
                tail_count: None,
                recover: make_full_recover(invocation_id, path),
            });
            return Value::String(prepared);
        }
        if let Some(elide) = byte_elide_string(&prepared, budget) {
            elided_paths.push(ElidedPath {
                path: path.to_string(),
                bytes: s.len() as u64,
                lines: None,
                length: None,
                head_count: None,
                tail_count: None,
                recover: make_range_recover(invocation_id, path, elide.head_end, elide.missing_len),
            });
            return Value::String(elide.text);
        }
        Value::String(prepared)
    }

    #[allow(clippy::too_many_arguments)]
    fn truncate_array(
        &self,
        walked: &[Value],
        original_length: usize,
        original_bytes: u64,
        path: &str,
        invocation_id: ToolInvocationId,
        paths_before: usize,
        elided_paths: &mut Vec<ElidedPath>,
    ) -> Value {
        let budget = self.sentinel_budget();
        let n = walked.len();
        let mut head_count = n / 2;
        let mut tail_count = n - head_count;
        if tail_count > 0 {
            tail_count -= 1;
        } else {
            head_count = head_count.saturating_sub(1);
        }
        let mut truncated = make_truncated_array(walked, head_count, tail_count);
        while (json_size(&truncated) as usize) > budget && (head_count > 0 || tail_count > 0) {
            if tail_count > head_count {
                tail_count -= 1;
            } else {
                head_count -= 1;
            }
            truncated = make_truncated_array(walked, head_count, tail_count);
        }
        // Surgical orphan cleanup: keep per-item elided_paths whose
        // $[i] index landed in head [0..head_count) or tail
        // [n-tail_count..n). Indices in between point at items that
        // dropped out of the kept shape — drop their recovery handles.
        let kept_low = head_count;
        let kept_high = n - tail_count;
        let after_walk: Vec<ElidedPath> = elided_paths.split_off(paths_before);
        for entry in after_walk {
            match parse_array_index(&entry.path, path) {
                Some(i) if i < kept_low || i >= kept_high => elided_paths.push(entry),
                None => elided_paths.push(entry),
                _ => {} // drop: this index is in the missing middle
            }
        }
        let missing_len = original_length.saturating_sub(head_count + tail_count);
        elided_paths.push(ElidedPath {
            path: path.to_string(),
            bytes: original_bytes,
            lines: None,
            length: Some(original_length as u32),
            head_count: Some(head_count as u32),
            tail_count: Some(tail_count as u32),
            recover: make_array_slice_recover(invocation_id, path, head_count, missing_len),
        });
        truncated
    }
}

/// Parse the immediate `$[N]` index out of a child elided_path, given
/// the container's own path. Returns `None` if the child path is not an
/// array-index descendant of the container.
fn parse_array_index(elided_path: &str, container_path: &str) -> Option<usize> {
    let rest = elided_path.strip_prefix(container_path)?;
    let inside = rest.strip_prefix('[')?;
    let close = inside.find(']')?;
    inside[..close].parse().ok()
}

/// Smallest value worth eliding. Below this, marker overhead exceeds
/// any byte savings — passthrough is strictly cheaper.
const MIN_ELIDE_BYTES: usize = 512;

fn json_size(value: &Value) -> u64 {
    serde_json::to_string(value)
        .map(|s| s.len() as u64)
        .unwrap_or(0)
}

fn line_count(s: &str) -> u32 {
    if s.is_empty() {
        0
    } else {
        (s.lines().count() as u32).max(1)
    }
}

// ── String elision: line-mode (preferred for line-oriented content) ──

/// `raw_offset` / `raw_missing` are line indices into the *persisted*
/// raw text (what `tool_output_fetch` slices). They're translated from
/// the post-chain "current" line indices via two composed mappings:
///
///   compacted → preprocessed   (`SegmentedText::current_to_original_line`)
///   preprocessed → raw         (`preprocessors::Preprocessed::preprocessed_to_raw`)
///
/// Both legs matter — chain passes compact `nix-copy`/`cargo`/`git-clone`
/// runs into XML markers, and the concourse preprocessor splits each
/// `\r`-packed raw line into one preprocessed line per intermediate
/// state. Skipping either leg desyncs the recovery template from the
/// on-disk bytes.
struct LineElide {
    text: String,
    raw_offset: u32,
    raw_missing: u32,
}

/// Translate a compacted-line index into a raw-line index by composing
/// the chain mapping and the preprocessor mapping. Out-of-range indices
/// resolve to the raw line count (i.e. "past the end"), which lets the
/// caller compute `raw_missing` as the difference between the head end
/// and the tail start without special-casing the right boundary.
fn compacted_to_raw_line(compacted: u32, ctx: &SegmentedText, preprocessed_to_raw: &[u32]) -> u32 {
    let preprocessed = ctx.current_to_original_line(compacted);
    let raw_total = preprocessed_to_raw.last().map(|&r| r + 1).unwrap_or(0);
    if (preprocessed as usize) >= preprocessed_to_raw.len() {
        raw_total
    } else {
        preprocessed_to_raw[preprocessed as usize]
    }
}

fn line_elide_string(
    s: &str,
    budget: usize,
    ctx: &SegmentedText,
    preprocessed_to_raw: &[u32],
) -> Option<LineElide> {
    let lines: Vec<&str> = s.lines().collect();
    let n = lines.len();
    if n < 3 {
        return None;
    }
    let mut head = n / 2;
    let mut tail = n - head;
    if tail > 0 {
        tail -= 1;
    } else {
        head = head.saturating_sub(1);
    }
    let mut elide = make_line_elide(&lines, head, tail, ctx, preprocessed_to_raw);
    while elide.text.len() > budget && (head > 0 || tail > 0) {
        if tail > head {
            tail -= 1;
        } else {
            head -= 1;
        }
        elide = make_line_elide(&lines, head, tail, ctx, preprocessed_to_raw);
    }
    if head == 0 && tail == 0 {
        return None;
    }
    Some(elide)
}

fn make_line_elide(
    lines: &[&str],
    head: usize,
    tail: usize,
    ctx: &SegmentedText,
    preprocessed_to_raw: &[u32],
) -> LineElide {
    let n = lines.len();
    let missing_compacted = n - head - tail;
    let raw_offset = compacted_to_raw_line(head as u32, ctx, preprocessed_to_raw);
    let raw_after =
        compacted_to_raw_line((head + missing_compacted) as u32, ctx, preprocessed_to_raw);
    // If the elided middle falls entirely inside a single raw line
    // (e.g. inner `\r`-progress segments of a packed line), return that
    // raw line as the recovery so the agent gets at least the
    // containing content instead of an empty slice.
    let raw_missing = raw_after.saturating_sub(raw_offset).max(1);
    let mut text = String::new();
    if head > 0 {
        let head_text = lines[..head].join("\n");
        text.push_str(&format!("<head lines=\"{head}\">\n{head_text}\n</head>\n"));
    }
    text.push_str(&format!(
        "<bulk-elided original-lines=\"{raw_missing}\">\n\
         {raw_missing} lines elided\n\
         </bulk-elided>\n"
    ));
    if tail > 0 {
        let tail_text = lines[n - tail..].join("\n");
        text.push_str(&format!("<tail lines=\"{tail}\">\n{tail_text}\n</tail>"));
    } else {
        text.truncate(text.trim_end_matches('\n').len());
    }
    LineElide {
        text,
        raw_offset,
        raw_missing,
    }
}

// ── String elision: byte-mode (fallback for single-line / few-line) ──

struct ByteElide {
    text: String,
    head_end: usize,
    missing_len: usize,
}

fn byte_elide_string(s: &str, budget: usize) -> Option<ByteElide> {
    // Marker overhead ~200 bytes — reserve, then split the rest evenly
    // between head and tail.
    const MARKER_OVERHEAD: usize = 200;
    let available = budget.saturating_sub(MARKER_OVERHEAD);
    let half = available / 2;
    let head_end = floor_char_boundary(s, half);
    let tail_target = s.len().saturating_sub(half);
    let tail_start = ceil_char_boundary(s, tail_target.max(head_end));
    if head_end >= tail_start {
        return None;
    }
    let missing_len = tail_start - head_end;
    let tail_bytes = s.len() - tail_start;
    let mut text = String::new();
    if head_end > 0 {
        text.push_str(&format!(
            "<head bytes=\"{head_end}\">\n{}\n</head>\n",
            &s[..head_end],
        ));
    }
    text.push_str(&format!(
        "<bulk-elided original-bytes=\"{missing_len}\">\n\
         {missing_len} bytes elided\n\
         </bulk-elided>\n"
    ));
    if tail_bytes > 0 {
        text.push_str(&format!(
            "<tail bytes=\"{tail_bytes}\">\n{}\n</tail>",
            &s[tail_start..],
        ));
    } else {
        text.truncate(text.trim_end_matches('\n').len());
    }
    Some(ByteElide {
        text,
        head_end,
        missing_len,
    })
}

fn floor_char_boundary(s: &str, idx: usize) -> usize {
    let idx = idx.min(s.len());
    let mut i = idx;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, idx: usize) -> usize {
    let idx = idx.min(s.len());
    let mut i = idx;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

// ── Recovery templates ──

fn make_range_recover(
    invocation_id: ToolInvocationId,
    path: &str,
    offset: usize,
    len: usize,
) -> Value {
    serde_json::json!({
        "tool": "tool_output_fetch",
        "args_template": {
            "invocation_id": invocation_id.to_string(),
            "path": path,
            "query": { "mode": "range", "offset": offset, "len": len },
        }
    })
}

fn make_lines_recover(
    invocation_id: ToolInvocationId,
    path: &str,
    offset: usize,
    len: usize,
) -> Value {
    serde_json::json!({
        "tool": "tool_output_fetch",
        "args_template": {
            "invocation_id": invocation_id.to_string(),
            "path": path,
            "query": { "mode": "lines", "offset": offset, "len": len },
        }
    })
}

fn make_full_recover(invocation_id: ToolInvocationId, path: &str) -> Value {
    serde_json::json!({
        "tool": "tool_output_fetch",
        "args_template": {
            "invocation_id": invocation_id.to_string(),
            "path": path,
        }
    })
}

fn make_array_slice_recover(
    invocation_id: ToolInvocationId,
    path: &str,
    offset: usize,
    len: usize,
) -> Value {
    serde_json::json!({
        "tool": "tool_output_fetch",
        "args_template": {
            "invocation_id": invocation_id.to_string(),
            "path": path,
            "query": { "mode": "json_array_slice", "offset": offset, "len": len },
        }
    })
}

fn make_truncated_array(walked: &[Value], head_count: usize, tail_count: usize) -> Value {
    let mut out: Vec<Value> = walked.iter().take(head_count).cloned().collect();
    out.extend(walked.iter().rev().take(tail_count).rev().cloned());
    Value::Array(out)
}
