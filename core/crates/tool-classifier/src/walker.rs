//! JSON-aware elision walker.

use serde_json::{Map, Value};

use super::string_summarizer::{SegmentedText, StringSummarizerChain};
use super::{ElidedPath, ElisionKind};

const STRING_ELIDE_HEAD_CHARS: usize = 256;
const STRING_ELIDE_TAIL_CHARS: usize = 256;
const STRING_ELIDE_MARKER: &str = "…";

const ARRAY_SENTINEL_HEAD: usize = 3;
const ARRAY_SENTINEL_TAIL: usize = 3;

const SENTINEL_HARD_CAP_BYTES: usize = 16 * 1024;
const SENTINEL_MIN_BYTES: usize = 512;

pub const RECOVERY_INVOCATION_PLACEHOLDER: &str = "<this-invocation>";

#[derive(Debug)]
pub enum WalkOutcome {
    Passthrough(Value),
    Elided {
        kept: Value,
        elided_paths: Vec<ElidedPath>,
        total_bytes: u64,
        kept_bytes: u32,
    },
}

pub fn classify_value(
    value: &Value,
    threshold: usize,
    chain: Option<&StringSummarizerChain>,
) -> WalkOutcome {
    let total_bytes = json_size(value);
    let mut paths = Vec::new();
    let mut typed_replacements = 0usize;
    let kept = walk(
        value,
        threshold,
        chain,
        "$".to_string(),
        &mut paths,
        &mut typed_replacements,
    );
    let kept_bytes = json_size(&kept);
    if kept == *value {
        return WalkOutcome::Passthrough(value.clone());
    }
    if typed_replacements == 0 && kept_bytes >= total_bytes {
        return WalkOutcome::Passthrough(value.clone());
    }
    let reported_kept = kept_bytes.min(total_bytes);
    WalkOutcome::Elided {
        kept,
        elided_paths: paths,
        total_bytes: total_bytes as u64,
        kept_bytes: reported_kept.min(u32::MAX as usize) as u32,
    }
}

fn walk(
    value: &Value,
    threshold: usize,
    chain: Option<&StringSummarizerChain>,
    path: String,
    paths: &mut Vec<ElidedPath>,
    typed_replacements: &mut usize,
) -> Value {
    match value {
        Value::String(s) => {
            if let Some(c) = chain {
                let mut ctx = SegmentedText::from_initial(s);
                let modified = c.run(&mut ctx);
                let current = ctx.into_log();
                if modified {
                    *typed_replacements += 1;
                    return Value::String(current);
                }
                if current != *s {
                    return Value::String(current);
                }
            }
            let bytes = json_size(value);
            if bytes >= threshold {
                byte_elide_string(s, &path, paths, bytes)
            } else {
                value.clone()
            }
        }
        Value::Array(items) => {
            let bytes = json_size(value);
            if bytes < threshold {
                return value.clone();
            }
            walk_array(
                items,
                threshold,
                chain,
                &path,
                paths,
                bytes,
                typed_replacements,
            )
        }
        Value::Object(map) => {
            let bytes = json_size(value);
            if bytes < threshold {
                return value.clone();
            }
            walk_object(
                map,
                threshold,
                chain,
                &path,
                paths,
                bytes,
                typed_replacements,
            )
        }
        _ => value.clone(),
    }
}

fn byte_elide_string(
    s: &str,
    path: &str,
    paths: &mut Vec<ElidedPath>,
    original_bytes: usize,
) -> Value {
    let head_end = floor_char_boundary(s, STRING_ELIDE_HEAD_CHARS);
    let tail_target = s.len().saturating_sub(STRING_ELIDE_TAIL_CHARS);
    let tail_start = ceil_char_boundary(s, tail_target.max(head_end));
    if head_end >= tail_start {
        return Value::String(s.to_string());
    }
    let elided = format!(
        "{}{}{}",
        &s[..head_end],
        STRING_ELIDE_MARKER,
        &s[tail_start..]
    );
    paths.push(ElidedPath {
        path: path.to_string(),
        kind: ElisionKind::String,
        bytes: original_bytes as u64,
        length: None,
        preview: None,
    });
    Value::String(elided)
}

fn walk_array(
    items: &[Value],
    threshold: usize,
    chain: Option<&StringSummarizerChain>,
    path: &str,
    paths: &mut Vec<ElidedPath>,
    original_bytes: usize,
    typed_replacements: &mut usize,
) -> Value {
    let paths_before = paths.len();
    let walked: Vec<Value> = items
        .iter()
        .enumerate()
        .map(|(i, v)| {
            walk(
                v,
                threshold,
                chain,
                format!("{path}[{i}]"),
                paths,
                typed_replacements,
            )
        })
        .collect();
    let walked_value = Value::Array(walked.clone());
    if json_size(&walked_value) < threshold {
        return walked_value;
    }
    paths.truncate(paths_before);
    sentinel_array(&walked, items.len(), path, paths, original_bytes, threshold)
}

fn sentinel_budget(threshold: usize) -> usize {
    threshold.clamp(SENTINEL_MIN_BYTES, SENTINEL_HARD_CAP_BYTES)
}

fn sentinel_array(
    walked_items: &[Value],
    original_length: usize,
    path: &str,
    paths: &mut Vec<ElidedPath>,
    original_bytes: usize,
    threshold: usize,
) -> Value {
    let budget = sentinel_budget(threshold);
    let mut head_count = ARRAY_SENTINEL_HEAD.min(walked_items.len());
    let max_tail = walked_items.len().saturating_sub(head_count);
    let mut tail_count = ARRAY_SENTINEL_TAIL.min(max_tail);
    let mut sentinel = make_array_sentinel(
        walked_items,
        head_count,
        tail_count,
        original_length,
        original_bytes,
        path,
    );
    while json_size(&sentinel) > budget && (head_count > 0 || tail_count > 0) {
        if tail_count > 0 {
            tail_count -= 1;
        } else {
            head_count -= 1;
        }
        sentinel = make_array_sentinel(
            walked_items,
            head_count,
            tail_count,
            original_length,
            original_bytes,
            path,
        );
    }
    paths.push(ElidedPath {
        path: path.to_string(),
        kind: ElisionKind::Array,
        bytes: original_bytes as u64,
        length: Some(original_length),
        preview: None,
    });
    sentinel
}

fn make_array_sentinel(
    walked_items: &[Value],
    head_count: usize,
    tail_count: usize,
    length: usize,
    bytes: usize,
    path: &str,
) -> Value {
    let head: Vec<Value> = walked_items.iter().take(head_count).cloned().collect();
    let tail: Vec<Value> = walked_items
        .iter()
        .rev()
        .take(tail_count)
        .rev()
        .cloned()
        .collect();
    let missing_offset = head_count;
    let missing_len = length.saturating_sub(head_count + tail_count);
    let recover = serde_json::json!({
        "tool": "tool_output_fetch",
        "args_template": {
            "invocation_id": RECOVERY_INVOCATION_PLACEHOLDER,
            "view": "original",
            "query": {
                "mode": "json_array_slice",
                "path": path,
                "offset": missing_offset,
                "len": missing_len,
            },
        },
    });
    serde_json::json!({
        "_elided": true,
        "kind": "array",
        "length": length,
        "bytes": bytes,
        "head": head,
        "tail": tail,
        "_recover": recover,
    })
}

fn walk_object(
    map: &Map<String, Value>,
    threshold: usize,
    chain: Option<&StringSummarizerChain>,
    path: &str,
    paths: &mut Vec<ElidedPath>,
    _original_bytes: usize,
    typed_replacements: &mut usize,
) -> Value {
    let mut walked: Map<String, Value> = Map::new();
    let mut per_key_paths: Vec<(String, Vec<ElidedPath>)> = Vec::with_capacity(map.len());
    let mut original_sizes: std::collections::HashMap<String, usize> =
        std::collections::HashMap::with_capacity(map.len());
    for (k, v) in map.iter() {
        original_sizes.insert(k.clone(), json_size(v));
        let mut child_paths = Vec::new();
        let walked_v = walk(
            v,
            threshold,
            chain,
            format!("{path}.{k}"),
            &mut child_paths,
            typed_replacements,
        );
        per_key_paths.push((k.clone(), child_paths));
        walked.insert(k.clone(), walked_v);
    }
    let walked_value = Value::Object(walked.clone());
    if json_size(&walked_value) < threshold {
        for (_, child_paths) in per_key_paths {
            paths.extend(child_paths);
        }
        return walked_value;
    }
    peel_object_keys(
        walked,
        threshold,
        path,
        paths,
        per_key_paths,
        original_sizes,
    )
}

fn peel_object_keys(
    mut walked: Map<String, Value>,
    threshold: usize,
    path: &str,
    paths: &mut Vec<ElidedPath>,
    mut per_key_paths: Vec<(String, Vec<ElidedPath>)>,
    original_sizes: std::collections::HashMap<String, usize>,
) -> Value {
    let mut current_size = json_size(&Value::Object(walked.clone()));
    loop {
        if current_size < threshold {
            break;
        }
        let candidate = walked
            .iter()
            .filter(|(_, v)| !is_sentinel(v))
            .filter(|(_, v)| !matches!(v, Value::String(_)))
            .map(|(k, v)| (k.clone(), json_size(v)))
            .max_by_key(|(_, size)| *size);
        let Some((key, post_walk_size)) = candidate else {
            break;
        };
        let value = walked
            .remove(&key)
            .expect("just selected this key from the same map");
        let (kind, length) = match &value {
            Value::String(_) => (ElisionKind::String, None),
            Value::Array(a) => (ElisionKind::Array, Some(a.len())),
            Value::Object(o) => (ElisionKind::Object, Some(o.len())),
            _ => {
                walked.insert(key, value);
                break;
            }
        };
        let original_size = original_sizes.get(&key).copied().unwrap_or(post_walk_size);
        per_key_paths.retain(|(k, _)| k != &key);
        paths.push(ElidedPath {
            path: format!("{path}.{key}"),
            kind,
            bytes: original_size as u64,
            length,
            preview: None,
        });
        let sentinel = sentinel_for(kind, original_size, length);
        let sentinel_size = json_size(&sentinel);
        walked.insert(key, sentinel);
        current_size = current_size
            .saturating_sub(post_walk_size)
            .saturating_add(sentinel_size);
    }
    for (_, child_paths) in per_key_paths {
        paths.extend(child_paths);
    }
    Value::Object(walked)
}

fn sentinel_for(kind: ElisionKind, bytes: usize, length: Option<usize>) -> Value {
    let kind_str = match kind {
        ElisionKind::String => "string",
        ElisionKind::Array => "array",
        ElisionKind::Object => "object",
    };
    let mut obj = serde_json::Map::new();
    obj.insert("_elided".to_string(), Value::Bool(true));
    obj.insert("kind".to_string(), Value::String(kind_str.to_string()));
    obj.insert("bytes".to_string(), Value::from(bytes));
    if let Some(n) = length {
        obj.insert("length".to_string(), Value::from(n));
    }
    Value::Object(obj)
}

fn is_sentinel(v: &Value) -> bool {
    matches!(v, Value::Object(o) if o.get("_elided") == Some(&Value::Bool(true)))
}

fn json_size(value: &Value) -> usize {
    serde_json::to_string(value).map(|s| s.len()).unwrap_or(0)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walker_never_inflates_serialized_form() {
        let v = serde_json::json!((0..400).collect::<Vec<_>>());
        let raw_bytes = json_size(&v);
        let outcome = classify_value(&v, 4096, None);
        match outcome {
            WalkOutcome::Passthrough(passed) => assert_eq!(passed, v),
            WalkOutcome::Elided { kept_bytes, .. } => {
                assert!((kept_bytes as usize) < raw_bytes);
            }
        }
    }

    #[test]
    fn walker_preserves_structure_to_leaf() {
        let huge = "x".repeat(20_000);
        let v = serde_json::json!({
            "a": { "b": { "c": { "d": huge } } }
        });
        let outcome = classify_value(&v, 4096, None);
        let WalkOutcome::Elided {
            kept,
            elided_paths,
            kept_bytes,
            total_bytes,
        } = outcome
        else {
            panic!("expected Elided");
        };
        assert!((kept_bytes as u64) < total_bytes);
        let leaf = kept
            .get("a")
            .and_then(|x| x.get("b"))
            .and_then(|x| x.get("c"))
            .and_then(|x| x.get("d"))
            .and_then(|v| v.as_str())
            .expect("leaf string preserved as String");
        assert!(leaf.len() < 20_000);
        assert_eq!(elided_paths.len(), 1);
        assert_eq!(elided_paths[0].path, "$.a.b.c.d");
        assert!(matches!(elided_paths[0].kind, ElisionKind::String));
    }

    #[test]
    fn walker_string_elision_preserves_type() {
        let huge = "y".repeat(10_000);
        let v = serde_json::json!({ "output": huge });
        let outcome = classify_value(&v, 4096, None);
        let WalkOutcome::Elided { kept, .. } = outcome else {
            panic!("expected Elided");
        };
        let output = kept.get("output").expect("key present");
        assert!(output.is_string());
        let s = output.as_str().unwrap();
        assert!(s.len() < 10_000);
        assert!(s.contains(STRING_ELIDE_MARKER));
    }

    #[test]
    fn walker_emits_array_sentinel_when_items_irreducible() {
        let items: Vec<Value> = (0..200)
            .map(|i| Value::String(format!("item-{i:03}-{}", "z".repeat(20))))
            .collect();
        let v = Value::Array(items);
        let outcome = classify_value(&v, 4096, None);
        let WalkOutcome::Elided {
            kept, elided_paths, ..
        } = outcome
        else {
            panic!("expected Elided");
        };
        let sentinel = kept.as_object().expect("sentinel is an object");
        assert_eq!(sentinel.get("_elided"), Some(&Value::Bool(true)));
        assert_eq!(
            sentinel.get("kind"),
            Some(&Value::String("array".to_string()))
        );
        assert_eq!(sentinel.get("length"), Some(&Value::from(200)));
        assert!(sentinel.contains_key("head"));
        assert!(sentinel.contains_key("tail"));
        assert!(elided_paths
            .iter()
            .any(|p| p.path == "$" && matches!(p.kind, ElisionKind::Array)));
    }

    #[test]
    fn array_sentinel_carries_recover_template_with_placeholder() {
        let items: Vec<Value> = (0..50)
            .map(|i| serde_json::json!({"id": i, "blob": "z".repeat(200)}))
            .collect();
        let v = serde_json::json!({"hits": items});
        let outcome = classify_value(&v, 4096, None);
        let WalkOutcome::Elided { kept, .. } = outcome else {
            panic!("expected Elided");
        };
        let hits = kept.get("hits").expect("hits key present");
        assert_eq!(hits.get("_elided"), Some(&Value::Bool(true)));
        let recover = hits.get("_recover").expect("_recover present");
        assert_eq!(
            recover.get("tool"),
            Some(&Value::String("tool_output_fetch".to_string()))
        );
        let args = recover.get("args_template").expect("args_template");
        assert_eq!(
            args.get("invocation_id"),
            Some(&Value::String(RECOVERY_INVOCATION_PLACEHOLDER.to_string()))
        );
        assert_eq!(
            args.get("view"),
            Some(&Value::String("original".to_string()))
        );
        let q = args.get("query").expect("query");
        assert_eq!(
            q.get("mode"),
            Some(&Value::String("json_array_slice".to_string()))
        );
        assert_eq!(q.get("path"), Some(&Value::String("$.hits".to_string())));
        assert!(q.get("offset").and_then(|v| v.as_u64()).is_some());
        assert!(q.get("len").and_then(|v| v.as_u64()).is_some());
    }

    #[test]
    fn floating_sentinel_budget_keeps_more_items_when_threshold_allows() {
        let items: Vec<Value> = (0..30)
            .map(|i| serde_json::json!({"id": i, "name": format!("entry-{i:02}")}))
            .collect();
        let v = serde_json::json!({"hits": items});
        let small_budget = classify_value(&v, 4096, None);
        let large_budget = classify_value(&v, 16 * 1024, None);
        match (small_budget, large_budget) {
            (
                WalkOutcome::Elided {
                    kept: small_kept, ..
                },
                WalkOutcome::Elided {
                    kept: large_kept, ..
                },
            ) => {
                let small_head = small_kept
                    .get("hits")
                    .and_then(|v| v.get("head"))
                    .and_then(|h| h.as_array())
                    .expect("small budget head");
                let large_head = large_kept
                    .get("hits")
                    .and_then(|v| v.get("head"))
                    .and_then(|h| h.as_array())
                    .expect("large budget head");
                assert!(
                    large_head.len() >= small_head.len(),
                    "larger threshold should keep at least as many head items \
                     (small={} large={})",
                    small_head.len(),
                    large_head.len(),
                );
            }
            outcomes => panic!("expected both Elided, got {outcomes:?}"),
        }
    }

    /// Cursor #3212527301: byte-elide reports original pre-walk size.
    #[test]
    fn elided_string_field_reports_original_pre_walk_bytes() {
        let original_string_size = 50_000;
        let mut obj = serde_json::Map::new();
        for i in 0..50 {
            obj.insert(
                format!("field_{i:02}"),
                Value::String("z".repeat(original_string_size)),
            );
        }
        let v = Value::Object(obj);
        let outcome = classify_value(&v, 4096, None);
        let WalkOutcome::Elided { elided_paths, .. } = outcome else {
            panic!("expected Elided");
        };
        let peeled = elided_paths
            .iter()
            .find(|p| {
                p.path.starts_with("$.field_")
                    && matches!(p.kind, ElisionKind::String)
                    && p.bytes > 1_000
            })
            .unwrap_or_else(|| {
                panic!(
                    "expected peeled field with bytes ≈ original; got: {:?}",
                    elided_paths
                        .iter()
                        .map(|p| (p.path.clone(), p.bytes))
                        .collect::<Vec<_>>(),
                )
            });
        assert!(peeled.bytes as usize >= original_string_size);
    }

    #[test]
    fn walker_peels_largest_object_key_when_recursion_didnt_help() {
        let huge_items: Vec<Value> = (0..200)
            .map(|i| Value::String(format!("entry-{i:03}-{}", "q".repeat(20))))
            .collect();
        let v = serde_json::json!({
            "meta": {"id": 42, "status": "ok"},
            "huge_array": huge_items,
            "summary": {"count": 200},
        });
        let outcome = classify_value(&v, 4096, None);
        let WalkOutcome::Elided {
            kept, elided_paths, ..
        } = outcome
        else {
            panic!("expected Elided");
        };
        assert_eq!(
            kept.get("meta").and_then(|m| m.get("id")),
            Some(&Value::from(42))
        );
        assert_eq!(
            kept.get("summary").and_then(|s| s.get("count")),
            Some(&Value::from(200))
        );
        assert!(elided_paths.iter().any(|p| p.path.contains("huge_array")));
    }

    #[test]
    fn walker_passthroughs_below_threshold() {
        let v = serde_json::json!({"hi": "world"});
        let outcome = classify_value(&v, 4096, None);
        match outcome {
            WalkOutcome::Passthrough(passed) => assert_eq!(passed, v),
            WalkOutcome::Elided { .. } => panic!("small input must Passthrough"),
        }
    }

    /// Cursor #3207731468: sentinel-replaced array drops orphan child paths.
    #[test]
    fn walker_no_orphaned_paths_under_array_sentinel() {
        let items: Vec<Value> = (0..200)
            .map(|i| serde_json::json!({"id": i, "output": "z".repeat(600)}))
            .collect();
        let v = Value::Array(items);
        let WalkOutcome::Elided {
            kept, elided_paths, ..
        } = classify_value(&v, 4096, None)
        else {
            panic!("expected Elided");
        };
        for p in &elided_paths {
            assert!(resolves(&kept, &p.path), "{} absent from kept", p.path);
        }
    }

    #[test]
    fn walker_no_orphaned_paths_under_peeled_object_key() {
        let huge_blob = "p".repeat(600);
        let big_array: Vec<Value> = (0..200)
            .map(|_| serde_json::json!({"line": huge_blob.clone()}))
            .collect();
        let v = serde_json::json!({
            "meta": {"id": 1, "tag": "a"},
            "huge": big_array,
        });
        let WalkOutcome::Elided {
            kept, elided_paths, ..
        } = classify_value(&v, 4096, None)
        else {
            panic!("expected Elided");
        };
        for p in &elided_paths {
            assert!(resolves(&kept, &p.path), "{} absent from kept", p.path);
        }
        assert!(elided_paths.iter().any(|p| p.path == "$.huge"));
        assert!(!elided_paths
            .iter()
            .any(|p| p.path.starts_with("$.huge[") || p.path.starts_with("$.huge.")));
    }

    fn resolves(kept: &Value, path: &str) -> bool {
        if path == "$" {
            return true;
        }
        let mut cur = kept;
        let mut s = path.strip_prefix('$').unwrap_or(path);
        while !s.is_empty() {
            if let Some(rest) = s.strip_prefix('.') {
                let (segment, tail) = split_at_object_or_index(rest);
                cur = match cur.get(segment) {
                    Some(v) => v,
                    None => return false,
                };
                s = tail;
            } else if let Some(rest) = s.strip_prefix('[') {
                let close = rest.find(']').unwrap_or(rest.len());
                let idx: usize = rest[..close].parse().unwrap_or(usize::MAX);
                cur = match cur.get(idx) {
                    Some(v) => v,
                    None => return false,
                };
                s = &rest[close + 1..];
            } else {
                return false;
            }
        }
        true
    }

    fn split_at_object_or_index(s: &str) -> (&str, &str) {
        for (i, c) in s.char_indices() {
            if c == '.' || c == '[' {
                return (&s[..i], &s[i..]);
            }
        }
        (s, "")
    }
}
