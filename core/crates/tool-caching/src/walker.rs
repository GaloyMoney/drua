use std::sync::Arc;

use serde_json::Value;

use crate::config::ToolCachingConfig;
use crate::fetch::fetch_text_size_at_path;
use crate::preprocessors;
use crate::primitives::{ElidedPath, QueryStructure, ToolCallSummary, ToolInvocationId};
use crate::string_summarizer::{SegmentedText, StringSummarizerChain};

/// Per-call walk state. `primary_path` is where the preprocessor's
/// cleaned text lives (e.g. `$.logs` for concourse). `primary_mapping`
/// is the per-output-line index back to raw bytes; only the string leaf
/// at `primary_path` uses it, every other leaf uses identity mapping.
/// `primary_raw_bytes` is the size of the raw upstream text at
/// `primary_path` — used as `total_bytes` on the primary leaf's
/// `ElidedPath` so `<summary>` and `<elided>` at the same path agree
/// on totals (without it, `s.len()` inside `summarize_string` would
/// report the preprocessed length).
struct WalkCtx<'a> {
    invocation_id: ToolInvocationId,
    primary_path: &'a str,
    primary_mapping: Option<&'a [u32]>,
    primary_raw_bytes: Option<u64>,
    primary_raw_string: Option<&'a str>,
    /// Mirror of the preprocessor's `prefer_tail`; only honored at `primary_path`.
    primary_prefer_tail: bool,
}

#[derive(Clone)]
pub struct Walker {
    chain: Arc<StringSummarizerChain>,
    threshold_bytes: usize,
    sentinel_min_bytes: usize,
    sentinel_hard_cap_bytes: usize,
    max_fetch_response_bytes: usize,
}

impl Walker {
    pub fn new(chain: Arc<StringSummarizerChain>, config: &ToolCachingConfig) -> Self {
        Self {
            chain,
            threshold_bytes: config.generic_threshold_bytes,
            sentinel_min_bytes: config.sentinel_min_bytes,
            sentinel_hard_cap_bytes: config.sentinel_hard_cap_bytes,
            max_fetch_response_bytes: config.max_fetch_response_bytes,
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
        // Preprocessor (if registered for this tool) owns its shape: it
        // returns a transformed root with text in-place cleaned, plus
        // the json-path where the cleaned text lives and the per-line
        // mapping back to raw bytes. Walker then walks that as-if it
        // were the natural input. Non-matched tools walk the raw root
        // with identity mapping.
        let preprocessed = preprocessors::preprocess(tool_name, &query_structure.root);
        let (root_for_walk, primary_path, primary_mapping): (&Value, &str, Option<&[u32]>) =
            match &preprocessed {
                Some(p) => (&p.root, p.root_path.as_str(), Some(&p.preprocessed_to_raw)),
                None => (&query_structure.root, "$", None),
            };
        // Raw upstream size at the primary path — needed so the ElidedPath
        // at that path reports raw byte size (matches what fetch returns),
        // not the preprocessor-stripped size.
        let primary_raw = navigate_path(&query_structure.root, primary_path);
        let primary_raw_bytes = if preprocessed.is_some() {
            primary_raw.map(byte_size)
        } else {
            None
        };
        let primary_raw_string = primary_raw.and_then(Value::as_str);
        let primary_prefer_tail = preprocessed.as_ref().is_some_and(|p| p.prefer_tail);

        let mut elided_paths = Vec::new();
        let ctx = WalkCtx {
            invocation_id,
            primary_path,
            primary_mapping,
            primary_raw_bytes,
            primary_raw_string,
            primary_prefer_tail,
        };
        let wire_result = self.walk(
            root_for_walk,
            "$",
            self.threshold_bytes,
            &ctx,
            &mut elided_paths,
        );

        // `<summary>` body is the value at `primary_path` (an `$.logs`
        // string for concourse, or the whole walked root when there's
        // no preprocessor). For the non-matched case primary_path == "$"
        // and this reduces to the wire result.
        let summary_value = navigate_path(&wire_result, primary_path).unwrap_or(&wire_result);
        let summary = summary_value.clone();
        let summary_root_path = primary_path.to_string();

        // `<summary>` total/shown describes whatever the body holds —
        // the primary leaf for preprocessor-matched calls, the full
        // walked root otherwise.
        let raw_at_primary =
            navigate_path(&query_structure.root, primary_path).unwrap_or(&query_structure.root);
        let total_bytes = byte_size(raw_at_primary);
        let shown_bytes = byte_size(&summary);

        let (total_items, shown_items) = match (raw_at_primary, &summary) {
            (Value::Array(orig), Value::Array(walked)) => {
                (Some(orig.len() as u32), Some(walked.len() as u32))
            }
            _ => (None, None),
        };
        let (total_lines, shown_lines) = if matches!(summary, Value::String(_)) {
            elided_paths
                .iter()
                .find(|e| e.path == summary_root_path)
                .and_then(|e| Some((e.total_lines?, e.shown_lines?)))
                .map(|(t, s)| (Some(t), Some(s)))
                .unwrap_or((None, None))
        } else {
            (None, None)
        };

        ToolCallSummary {
            summary,
            wire_result,
            elided_paths,
            root_path: summary_root_path,
            total_bytes,
            shown_bytes,
            total_items,
            shown_items,
            total_lines,
            shown_lines,
        }
    }

    fn walk(
        &self,
        value: &Value,
        path: &str,
        budget: usize,
        ctx: &WalkCtx,
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
            Value::String(s) => self.summarize_string(s, path, budget, ctx, elided_paths),
            // Arrays / objects passthrough when sub-budget — their
            // children's chain passes can't help if the whole container
            // already fits.
            Value::Array(items) if size <= budget => Value::Array(items.clone()),
            Value::Object(map) if size <= budget => Value::Object(map.clone()),
            Value::Array(items) => self.walk_array(items, path, budget, ctx, elided_paths),
            Value::Object(map) => self.walk_object(map, path, budget, ctx, elided_paths),
            other => other.clone(),
        }
    }

    fn walk_array(
        &self,
        items: &[Value],
        path: &str,
        budget: usize,
        ctx: &WalkCtx,
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
            .map(|(i, v)| self.walk(v, &format!("{path}[{i}]"), per_item, ctx, elided_paths))
            .collect();
        let walked_value = Value::Array(walked.clone());
        if (json_size(&walked_value) as usize) <= budget {
            return walked_value;
        }
        // 0/1-item arrays can't be meaningfully sentinel'd.
        if n <= 1 {
            return walked_value;
        }
        // Schema-conforming truncate: emit a shorter (head-only) array of
        // the same element type so the wrapped outputSchema's `result`
        // stays valid. Truncation metadata moves into the ElidedPath,
        // where the structured envelope's `_elided.paths[i]` carries it.
        // Per-item elided_paths for kept items (e.g. $[0].body) survive;
        // those for dropped tail indices get pruned (or kept, if the
        // recovery slice wouldn't fit max_fetch_response_bytes — see
        // truncate_array).
        self.truncate_array(
            &walked,
            items,
            n,
            original_bytes,
            path,
            ctx.invocation_id,
            paths_before,
            elided_paths,
        )
    }

    fn walk_object(
        &self,
        map: &serde_json::Map<String, Value>,
        path: &str,
        budget: usize,
        ctx: &WalkCtx,
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
                    self.walk(v, &format!("{path}.{k}"), per_key, ctx, elided_paths),
                )
            })
            .collect();
        Value::Object(walked)
    }

    fn summarize_string(
        &self,
        s: &str,
        path: &str,
        budget: usize,
        ctx: &WalkCtx,
        elided_paths: &mut Vec<ElidedPath>,
    ) -> Value {
        let invocation_id = ctx.invocation_id;
        // Preprocessor already ran at the root (concourse strips ANSI,
        // timestamps, reflows \r-progress); the walker only needs its
        // line mapping at the preprocessor's primary path. For other
        // paths the mapping is identity.
        let is_primary = path == ctx.primary_path;
        let preprocessed_to_raw: Vec<u32> = match (is_primary, ctx.primary_mapping) {
            (true, Some(m)) => m.to_vec(),
            _ => preprocessors::identity_mapping(s),
        };
        // At the primary path the raw upstream bytes lived in
        // `ctx.primary_raw_bytes` (set by `summarize` before the walk).
        // Anywhere else, `s.len()` is the raw size — no preprocessor
        // touched it.
        let total_bytes_at_path: u64 = match (is_primary, ctx.primary_raw_bytes) {
            (true, Some(n)) => n,
            _ => s.len() as u64,
        };
        let persisted_string = match (is_primary, ctx.primary_raw_string) {
            (true, Some(raw)) => raw,
            _ => s,
        };
        // Pattern passes (nix-copy, cargo-compile, rsync, …) compact runs
        // of boring lines in place. Operates on a SegmentedText so order
        // and line indices stay accurate across passes.
        let mut ctx_seg = SegmentedText::from_initial(s);
        let _ = self.chain.run(&mut ctx_seg);
        let prepared = ctx_seg.log().to_string();
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
                total_bytes: total_bytes_at_path,
                shown_bytes: prepared.len() as u64,
                total_lines: Some(line_count(s)),
                shown_lines: Some(line_count(&prepared)),
                total_items: None,
                shown_items: None,
                recover: make_safe_full_recover(
                    invocation_id,
                    path,
                    persisted_string,
                    self.max_fetch_response_bytes,
                ),
            });
            return Value::String(prepared);
        }
        // Try line-mode first if the string has enough lines to split.
        let prefer_tail = is_primary && ctx.primary_prefer_tail;
        if let Some(elide) = line_elide_string(
            &prepared,
            budget,
            &ctx_seg,
            &preprocessed_to_raw,
            prefer_tail,
        ) {
            elided_paths.push(ElidedPath {
                path: path.to_string(),
                total_bytes: total_bytes_at_path,
                shown_bytes: elide.text.len() as u64,
                total_lines: Some(line_count(s)),
                shown_lines: Some(elide.shown_lines),
                total_items: None,
                shown_items: None,
                recover: make_safe_lines_recover(
                    invocation_id,
                    path,
                    persisted_string,
                    elide.raw_offset as usize,
                    elide.raw_missing as usize,
                    self.max_fetch_response_bytes,
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
                total_bytes: total_bytes_at_path,
                shown_bytes: prepared.len() as u64,
                total_lines: None,
                shown_lines: None,
                total_items: None,
                shown_items: None,
                recover: make_safe_full_recover(
                    invocation_id,
                    path,
                    persisted_string,
                    self.max_fetch_response_bytes,
                ),
            });
            return Value::String(prepared);
        }
        if let Some(elide) = byte_elide_string(&prepared, budget) {
            elided_paths.push(ElidedPath {
                path: path.to_string(),
                total_bytes: total_bytes_at_path,
                shown_bytes: elide.text.len() as u64,
                total_lines: None,
                shown_lines: None,
                total_items: None,
                shown_items: None,
                recover: make_safe_range_recover(
                    invocation_id,
                    path,
                    persisted_string,
                    elide.head_end,
                    elide.missing_len,
                    self.max_fetch_response_bytes,
                ),
            });
            return Value::String(elide.text);
        }
        Value::String(prepared)
    }

    #[allow(clippy::too_many_arguments)]
    fn truncate_array(
        &self,
        walked: &[Value],
        originals: &[Value],
        original_length: usize,
        original_bytes: u64,
        path: &str,
        invocation_id: ToolInvocationId,
        paths_before: usize,
        elided_paths: &mut Vec<ElidedPath>,
    ) -> Value {
        // Head-only truncate: emit `[item0..item(K-1)]` and record
        // `_elided.paths[i]={length:N, head_count:K, recover:…}` so the
        // agent can slice `[K..N)` via `tool_output_fetch` if they need
        // more. Head+tail would be ambiguous on the wire — without a
        // delimiter the agent can't tell where the gap lives.
        let budget = self.sentinel_budget();
        let n = walked.len();
        let mut head_count = n.saturating_sub(1);
        let mut truncated = make_truncated_array(walked, head_count);
        while (json_size(&truncated) as usize) > budget && head_count > 0 {
            head_count -= 1;
            truncated = make_truncated_array(walked, head_count);
        }

        // Validate the slice-mode recovery template against the FETCH
        // cap by sizing the *original* tail items (what
        // `tool_output_fetch` would actually return — it reads from the
        // persisted root, not the walked tree). If the slice would
        // exceed the cap, we'd be advertising a recovery call that's
        // guaranteed to fail with `FetchResponseTooLarge`. Switch to
        // per-index elided paths + summary-mode aggregate recover
        // instead — both are bounded by construction.
        let tail_originals_size =
            json_size(&Value::Array(originals[head_count..].to_vec())) as usize;
        let slice_fits_fetch_cap = tail_originals_size <= self.max_fetch_response_bytes;

        // Per-item elided_paths emitted during walk:
        //   - kept indices `[0..head_count)`: keep (still in wire shape)
        //   - dropped indices `[head_count..n)`: KEEP when slice
        //     wouldn't fit fetch cap (the agent needs them as per-index
        //     drill-downs); DROP otherwise (slice recovery covers the
        //     whole tail in one call).
        let after_walk: Vec<ElidedPath> = elided_paths.split_off(paths_before);
        for entry in after_walk {
            match parse_array_index(&entry.path, path) {
                Some(i) if i < head_count => elided_paths.push(entry),
                None => elided_paths.push(entry),
                Some(_) if !slice_fits_fetch_cap => elided_paths.push(entry),
                _ => {} // drop: this index is in the slice-recovered tail
            }
        }
        let missing_len = original_length.saturating_sub(head_count);
        let shown_bytes = json_size(&truncated);
        let recover = if slice_fits_fetch_cap {
            make_array_slice_recover(invocation_id, path, head_count, missing_len)
        } else {
            make_summary_recover(invocation_id, path)
        };
        elided_paths.push(ElidedPath {
            path: path.to_string(),
            total_bytes: original_bytes,
            shown_bytes,
            total_lines: None,
            shown_lines: None,
            total_items: Some(original_length as u32),
            shown_items: Some(head_count as u32),
            recover,
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

/// Type-aware byte size used for the root summary's `total-bytes` /
/// `shown-bytes`. Strings report raw byte length (no JSON quoting /
/// escape inflation), so the per-path `<elided>` tag — which also uses
/// `s.len()` for strings — and the `<summary>` tag agree on the count
/// for the same path. Arrays/objects fall back to JSON-serialized size
/// (no "natural" alternative representation).
fn byte_size(value: &Value) -> u64 {
    match value {
        Value::String(s) => s.len() as u64,
        other => json_size(other),
    }
}

/// Navigate `value` via a `$`-rooted json-path with only key / `[index]`
/// segments. Returns the sub-value at that path, or `None` if any
/// segment doesn't resolve. `$` returns `value` itself. Tolerant of
/// missing/typed mismatches — the caller falls back when `None`.
fn navigate_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let rest = path.strip_prefix('$')?;
    let mut cur = value;
    let mut chars = rest.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '.' => {
                // Key segment: until next `.` or `[` or end.
                let start = i + 1;
                let mut end = rest.len();
                for (j, c2) in rest[start..].char_indices() {
                    if c2 == '.' || c2 == '[' {
                        end = start + j;
                        break;
                    }
                }
                let key = &rest[start..end];
                cur = cur.as_object()?.get(key)?;
                // Resume after `end`.
                while let Some(&(p, _)) = chars.peek() {
                    if p < end {
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
            '[' => {
                // Array index: until `]`.
                let start = i + 1;
                let end = rest[start..].find(']').map(|j| start + j)?;
                let idx: usize = rest[start..end].parse().ok()?;
                cur = cur.as_array()?.get(idx)?;
                while let Some(&(p, _)) = chars.peek() {
                    if p <= end {
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
            _ => return None,
        }
    }
    Some(cur)
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
    /// Compacted-line count kept in `text` (head + tail). The bulk-elided
    /// marker block itself isn't counted — it's metadata, not data.
    shown_lines: u32,
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
    prefer_tail: bool,
) -> Option<LineElide> {
    let lines: Vec<&str> = s.lines().collect();
    let n = lines.len();
    if n < 3 {
        return None;
    }
    let (mut head, mut tail) = if prefer_tail {
        // CI-log shape: bias 1:4 head:tail so failures at the end stay
        // visible. The shrink loop below preserves the bias by trimming
        // head first when prefer_tail is set.
        let tail = (n * 4 / 5).max(1);
        let head = n.saturating_sub(tail + 1);
        (head, tail)
    } else {
        let head = n / 2;
        let tail = (n - head).saturating_sub(1);
        (head, tail)
    };
    let mut elide = make_line_elide(&lines, head, tail, ctx, preprocessed_to_raw);
    while elide.text.len() > budget && (head > 0 || tail > 0) {
        if prefer_tail {
            // Drain head first; only trim tail once head is exhausted.
            if head > 0 {
                head -= 1;
            } else {
                tail -= 1;
            }
        } else if tail > head {
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
        "<bulk-elided lines=\"{raw_missing}\">\n\
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
        shown_lines: (head + tail) as u32,
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
        "<bulk-elided bytes=\"{missing_len}\">\n\
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

fn make_safe_range_recover(
    invocation_id: ToolInvocationId,
    path: &str,
    raw: &str,
    offset: usize,
    len: usize,
    max_fetch_response_bytes: usize,
) -> Value {
    if fetch_string_range_fits(path, raw, offset, len, max_fetch_response_bytes) {
        return make_range_recover(invocation_id, path, offset, len);
    }
    let safe_len = max_safe_range_len(path, raw, offset, len, max_fetch_response_bytes);
    if safe_len == 0 {
        return make_summary_recover(invocation_id, path);
    }
    let mut recover = make_range_recover(invocation_id, path, offset, safe_len);
    // next_offset must land on a char boundary consistent with how
    // resolve_utf8_byte_window computes the chunk end (floor). Without this,
    // a multi-byte character straddling offset+safe_len is excluded from both
    // the current chunk (floor moves before it) and the next (ceil moves after).
    let next_offset = floor_char_boundary(raw, offset + safe_len);
    annotate_chunked_recover(
        &mut recover,
        "chunked_range",
        format!(
            "This bounded recovery returns the first {safe_len} of {len} elided bytes; repeat with a later offset to continue."
        ),
        len,
        next_offset,
    );
    recover
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

fn make_safe_lines_recover(
    invocation_id: ToolInvocationId,
    path: &str,
    raw: &str,
    offset: usize,
    len: usize,
    max_fetch_response_bytes: usize,
) -> Value {
    if fetch_string_lines_fits(path, raw, offset, len, max_fetch_response_bytes) {
        return make_lines_recover(invocation_id, path, offset, len);
    }
    let safe_len = max_safe_lines_len(path, raw, offset, len, max_fetch_response_bytes);
    if safe_len > 0 {
        let mut recover = make_lines_recover(invocation_id, path, offset, safe_len);
        annotate_chunked_recover(
            &mut recover,
            "chunked_lines",
            format!(
                "This bounded recovery returns the first {safe_len} of {len} elided lines; repeat with a later offset to continue."
            ),
            len,
            offset + safe_len,
        );
        return recover;
    }

    // A single line can exceed the fetch cap. Fall back to a byte-window
    // recovery scoped to the elided line span so the advertised call
    // still succeeds and does not point into already-shown tail content.
    let start = byte_offset_for_line(raw, offset);
    let end = byte_offset_for_line(raw, offset.saturating_add(len));
    let span_len = end.saturating_sub(start);
    if span_len == 0 {
        return make_summary_recover(invocation_id, path);
    }
    make_safe_range_recover(
        invocation_id,
        path,
        raw,
        start,
        span_len,
        max_fetch_response_bytes,
    )
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

fn make_safe_full_recover(
    invocation_id: ToolInvocationId,
    path: &str,
    raw: &str,
    max_fetch_response_bytes: usize,
) -> Value {
    if fetch_string_value_fits(path, raw, max_fetch_response_bytes) {
        make_full_recover(invocation_id, path)
    } else {
        make_summary_recover(invocation_id, path)
    }
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

/// Aggregate fallback marker for elisions whose direct slice would
/// exceed `max_fetch_response_bytes`. This is intentionally not
/// path-scoped: `mode:"summary"` replays the invocation's root summary
/// so callers that only have the structured manifest can inspect the
/// narrower `<elided>` pointers and drill down individually.
fn make_summary_recover(invocation_id: ToolInvocationId, scope_path: &str) -> Value {
    serde_json::json!({
        "tool": "tool_output_fetch",
        "kind": "aggregate_summary",
        "scope_path": scope_path,
        "note": "Direct recovery for this aggregate would exceed the fetch response cap; use the narrower recovery paths listed in this envelope. Summary mode only replays this overview.",
        "args_template": {
            "invocation_id": invocation_id.to_string(),
            "query": { "mode": "summary" },
        }
    })
}

fn annotate_chunked_recover(
    recover: &mut Value,
    kind: &'static str,
    note: String,
    total_len: usize,
    next_offset: usize,
) {
    if let Value::Object(map) = recover {
        map.insert("kind".to_string(), Value::String(kind.to_string()));
        map.insert("note".to_string(), Value::String(note));
        map.insert("complete".to_string(), Value::Bool(false));
        map.insert(
            "total_recovery_len".to_string(),
            Value::Number(serde_json::Number::from(total_len)),
        );
        map.insert(
            "next_offset".to_string(),
            Value::Number(serde_json::Number::from(next_offset)),
        );
    }
}

fn fetch_string_lines_fits(
    path: &str,
    raw: &str,
    offset: usize,
    len: usize,
    max_fetch_response_bytes: usize,
) -> bool {
    let slice = lines_slice(raw, offset, len);
    fetch_string_value_fits(path, &slice, max_fetch_response_bytes)
}

fn max_safe_lines_len(
    path: &str,
    raw: &str,
    offset: usize,
    len: usize,
    max_fetch_response_bytes: usize,
) -> usize {
    let mut lo = 0;
    let mut hi = len;
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if fetch_string_lines_fits(path, raw, offset, mid, max_fetch_response_bytes) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

fn fetch_string_range_fits(
    path: &str,
    raw: &str,
    offset: usize,
    len: usize,
    max_fetch_response_bytes: usize,
) -> bool {
    let (start, end) = resolve_utf8_byte_window(raw, offset, len);
    fetch_string_value_fits(path, &raw[start..end], max_fetch_response_bytes)
}

fn max_safe_range_len(
    path: &str,
    raw: &str,
    offset: usize,
    len: usize,
    max_fetch_response_bytes: usize,
) -> usize {
    let mut lo = 0;
    let mut hi = len;
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if fetch_string_range_fits(path, raw, offset, mid, max_fetch_response_bytes) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

fn fetch_string_value_fits(path: &str, value: &str, max_fetch_response_bytes: usize) -> bool {
    fetch_string_text_size(path, value) <= max_fetch_response_bytes
}

fn fetch_string_text_size(path: &str, value: &str) -> usize {
    fetch_text_size_at_path(path, Value::String(value.to_string())).unwrap_or(usize::MAX)
}

fn lines_slice(raw: &str, offset: usize, len: usize) -> String {
    let all: Vec<&str> = raw.lines().collect();
    let start = offset.min(all.len());
    let end = start.saturating_add(len).min(all.len());
    all[start..end].join("\n")
}

fn byte_offset_for_line(raw: &str, line: usize) -> usize {
    if line == 0 {
        return 0;
    }
    let mut count = 0;
    for (idx, ch) in raw.char_indices() {
        if ch == '\n' {
            count += 1;
            if count == line {
                return idx + 1;
            }
        }
    }
    raw.len()
}

fn resolve_utf8_byte_window(s: &str, offset: usize, len: usize) -> (usize, usize) {
    let requested_start = offset.min(s.len());
    let requested_end = requested_start.saturating_add(len).min(s.len());
    let start = ceil_char_boundary(s, requested_start);
    let end = floor_char_boundary(s, requested_end).max(start);
    (start, end)
}

fn make_truncated_array(walked: &[Value], head_count: usize) -> Value {
    Value::Array(walked.iter().take(head_count).cloned().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::QueryStructure;

    fn walker_with_fetch_cap(max_fetch: usize) -> Walker {
        let config = ToolCachingConfig {
            // Force array-truncation by making the array budget small.
            generic_threshold_bytes: 512,
            sentinel_min_bytes: 512,
            sentinel_hard_cap_bytes: 1024,
            max_fetch_response_bytes: max_fetch,
        };
        Walker::new(Arc::new(StringSummarizerChain::new()), &config)
    }

    /// Object of `n_fields` numeric fields — exceeds the array budget
    /// but stays incompressible so the walker keeps full per-item size.
    fn incompressible_item(seed: u32, n_fields: u32) -> Value {
        let mut obj = serde_json::Map::new();
        for k in 0..n_fields {
            obj.insert(format!("field_{seed}_{k}"), serde_json::json!(k as i64));
        }
        Value::Object(obj)
    }

    /// Tail of originals exceeds max_fetch_response_bytes → aggregate
    /// recover must switch to summary mode (slice would have produced
    /// a "fetch response too large" error).
    #[test]
    fn fat_tail_array_emits_summary_recover_not_slice() {
        // 5 items × ~600 bytes each = ~3KB array; walker budget 512
        // forces head_count=0 (or 1) so the tail covers all/most items.
        // Each item is ~600 bytes of incompressible numeric data — tail
        // size as fetched from persisted root would be ~3KB, well above
        // the 1KB fetch cap below.
        let items: Vec<Value> = (0..5).map(|i| incompressible_item(i, 40)).collect();
        let qs = QueryStructure {
            root: Value::Array(items),
        };
        let walker = walker_with_fetch_cap(1024);
        let summary = walker.summarize(&qs, ToolInvocationId::new(), "test_tool");

        // Find the aggregate `$` elided path (the truncation marker).
        let agg = summary
            .elided_paths
            .iter()
            .find(|e| e.path == "$")
            .unwrap_or_else(|| {
                panic!(
                    "aggregate elided path at $ missing; got paths: {:?}",
                    summary
                        .elided_paths
                        .iter()
                        .map(|e| &e.path)
                        .collect::<Vec<_>>()
                )
            });
        let mode = agg
            .recover
            .get("args_template")
            .and_then(|t| t.get("query"))
            .and_then(|q| q.get("mode"))
            .and_then(Value::as_str)
            .expect("recover has query.mode");
        assert_eq!(
            mode, "summary",
            "fat tail should switch aggregate recover to summary mode; got {mode}"
        );
        assert_eq!(
            agg.recover.get("kind").and_then(Value::as_str),
            Some("aggregate_summary")
        );
        assert_eq!(
            agg.recover.get("scope_path").and_then(Value::as_str),
            Some("$")
        );
        assert!(agg
            .recover
            .get("note")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .contains("narrower recovery paths"));
    }

    /// When the tail slice does fit the fetch cap, existing slice-mode
    /// recover stays in place (no regression).
    #[test]
    fn slim_tail_array_still_uses_slice_recover() {
        // 20 small items (~30 bytes each) → array ~700 bytes, > budget
        // 512, triggering truncate_array. Tail (any subset) easily fits
        // the generous 16KB fetch cap.
        let items: Vec<Value> = (0..20).map(|i| incompressible_item(i, 2)).collect();
        let qs = QueryStructure {
            root: Value::Array(items),
        };
        let walker = walker_with_fetch_cap(16 * 1024);
        let summary = walker.summarize(&qs, ToolInvocationId::new(), "test_tool");

        let agg = summary
            .elided_paths
            .iter()
            .find(|e| e.path == "$")
            .unwrap_or_else(|| {
                panic!(
                    "aggregate elided path at $ missing; got paths: {:?}",
                    summary
                        .elided_paths
                        .iter()
                        .map(|e| &e.path)
                        .collect::<Vec<_>>()
                )
            });
        let mode = agg
            .recover
            .get("args_template")
            .and_then(|t| t.get("query"))
            .and_then(|q| q.get("mode"))
            .and_then(Value::as_str)
            .expect("recover has query.mode");
        assert_eq!(mode, "json_array_slice");
        assert_eq!(
            agg.recover
                .get("args_template")
                .and_then(|t| t.get("path"))
                .and_then(Value::as_str),
            Some("$"),
            "path-scoped array slice recoveries advertise the elided path"
        );
    }

    fn walker_for_line_split() -> Walker {
        // Threshold so 100 short numbered lines (~700-800 bytes) clearly
        // overflow and trigger line_elide_string.
        let config = ToolCachingConfig {
            generic_threshold_bytes: 200,
            sentinel_min_bytes: 200,
            sentinel_hard_cap_bytes: 200,
            max_fetch_response_bytes: 16 * 1024,
        };
        Walker::new(Arc::new(StringSummarizerChain::new()), &config)
    }

    fn numbered_lines(n: u32) -> String {
        (0..n)
            .map(|i| format!("line_{i:03}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Concourse preprocessor sets `prefer_tail: true` — the walker's
    /// head/tail split must skew toward the tail so failure traces at
    /// the end of a build log stay visible.
    #[test]
    fn concourse_log_summary_is_tail_biased() {
        let logs = numbered_lines(100);
        let qs = QueryStructure {
            root: serde_json::json!({ "logs": logs }),
        };
        let walker = walker_for_line_split();
        let summary = walker.summarize(&qs, ToolInvocationId::new(), "concourse-build-log");
        let body = summary.summary.as_str().expect("logs summary is string");
        let head_count = body
            .lines()
            .filter(|l| l.starts_with("line_0") || l.starts_with("line_00"))
            .filter(|l| {
                let n: u32 = l.trim_start_matches("line_").parse().unwrap_or(u32::MAX);
                n < 50
            })
            .count();
        let tail_count = body
            .lines()
            .filter(|l| l.starts_with("line_"))
            .filter(|l| {
                let n: u32 = l.trim_start_matches("line_").parse().unwrap_or(u32::MAX);
                n >= 50
            })
            .count();
        assert!(
            tail_count > head_count,
            "tail-biased split should retain more tail than head lines; got head={head_count}, tail={tail_count}"
        );
        assert!(
            body.contains("line_099"),
            "last log line should survive tail-biased split"
        );
    }

    /// Non-concourse, non-preprocessed tools keep the symmetric ~50/50
    /// head/tail split (no regression on generic text).
    #[test]
    fn generic_string_summary_keeps_balanced_split() {
        let logs = numbered_lines(100);
        let qs = QueryStructure {
            root: Value::String(logs),
        };
        let walker = walker_for_line_split();
        let summary = walker.summarize(&qs, ToolInvocationId::new(), "unknown_tool");
        let body = summary.summary.as_str().expect("summary is string");
        let head_count = body
            .lines()
            .filter(|l| l.starts_with("line_"))
            .filter(|l| {
                let n: u32 = l.trim_start_matches("line_").parse().unwrap_or(u32::MAX);
                n < 50
            })
            .count();
        let tail_count = body
            .lines()
            .filter(|l| l.starts_with("line_"))
            .filter(|l| {
                let n: u32 = l.trim_start_matches("line_").parse().unwrap_or(u32::MAX);
                n >= 50
            })
            .count();
        let diff = head_count.abs_diff(tail_count);
        assert!(
            diff <= 1,
            "generic split should be ~symmetric; got head={head_count}, tail={tail_count}"
        );
    }

    /// Off-by-default helper: regenerates the bats `<summary>+<recovery>`
    /// golden files for fixtures the walker touches, by driving it over
    /// the raw input. Run with `REGEN_GOLDEN=1 cargo test -p
    /// drua-tool-caching --lib walker::tests::regen_bats_goldens --
    /// --nocapture`.
    #[test]
    fn regen_bats_goldens() {
        if std::env::var("REGEN_GOLDEN").is_err() {
            return;
        }
        for (fixture_name, tool_name) in [
            ("concourse-build-log", "fake_upstream_concourse-build-log"),
            ("arr-large-fat-items", "fake_upstream_arr-large-fat-items"),
        ] {
            regen_one(fixture_name, tool_name);
        }
    }

    #[test]
    fn chunked_range_next_offset_lands_on_char_boundary() {
        // "aaa…🔥bbb…" — the 4-byte emoji sits at bytes 50..54.
        // With fetch cap = 50, the binary search in max_safe_range_len settles on
        // safe_len = 53 (floor_char_boundary(53) = 50 bytes of content fits exactly).
        // Before fix: next_offset = 53 (mid-emoji) → ceil rounds to 54 → emoji lost.
        // After fix: next_offset = floor(53) = 50 → second chunk starts there.
        let prefix = "a".repeat(50);
        let emoji = "🔥"; // 4 bytes
        let suffix = "b".repeat(50);
        let raw = format!("{prefix}{emoji}{suffix}");
        let emoji_start = prefix.len();

        let path = "$";
        let inv = ToolInvocationId::new();
        // At path "$" the fetch size equals the byte length of the slice.
        // Cap=50 means only the 50 ASCII "a" chars fit; the emoji pushes over.
        let cap = 50;

        let recover = make_safe_range_recover(inv, path, &raw, 0, raw.len(), cap);

        let next_offset = recover
            .get("next_offset")
            .and_then(Value::as_u64)
            .expect("chunked recover has next_offset") as usize;

        assert!(
            raw.is_char_boundary(next_offset),
            "next_offset {next_offset} is not a char boundary"
        );
        assert!(
            next_offset <= emoji_start,
            "next_offset ({next_offset}) should be at or before emoji_start ({emoji_start})"
        );

        // Two-chunk fetch must reconstruct the full string without gaps.
        let (s1, e1) = resolve_utf8_byte_window(&raw, 0, next_offset);
        let (s2, e2) = resolve_utf8_byte_window(&raw, next_offset, raw.len() - next_offset);
        let reassembled = format!("{}{}", &raw[s1..e1], &raw[s2..e2]);
        assert_eq!(
            reassembled, raw,
            "two-chunk fetch must reconstruct the full string without gaps"
        );
    }

    fn regen_one(fixture_name: &str, tool_name: &str) {
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let fixture_path = manifest.join(format!(
            "../../../lib/fake-mcp-upstream/fixtures/{fixture_name}.json"
        ));
        let fixture: Value =
            serde_json::from_str(&std::fs::read_to_string(&fixture_path).expect("read fixture"))
                .expect("parse fixture");
        let root = match fixture["upstream"]["structured_content"].clone() {
            Value::Null => {
                // Fall back to parsing content[0].text the same way
                // ToolCaching::process() does for text-only upstreams.
                let text = fixture["upstream"]["content"][0]["text"]
                    .as_str()
                    .expect("content[0].text missing");
                QueryStructure::new(text).root
            }
            other => other,
        };
        let chain = Arc::new(crate::summarizer_passes::default_chain());
        let walker = Walker::new(chain, &crate::config::ToolCachingConfig::default());
        let qs = QueryStructure { root };
        let summary = walker.summarize(&qs, ToolInvocationId::new(), tool_name);
        let envelope = summary.build_envelope_text();
        let uuid_re = regex::Regex::new(
            r#"invocation_id="[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}""#,
        )
        .unwrap();
        let normalized = uuid_re
            .replace_all(&envelope, r#"invocation_id="<uuid>""#)
            .into_owned();
        let golden = manifest.join(format!(
            "../../../bats/summarized-tool-responses/{fixture_name}.txt"
        ));
        let trimmed = normalized.trim_end_matches('\n');
        std::fs::write(&golden, trimmed).expect("write golden");
        eprintln!("wrote {}", golden.display());
    }
}
