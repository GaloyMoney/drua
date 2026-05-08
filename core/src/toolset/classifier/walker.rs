//! JSON-aware elision walker — depth-first descent that byte-elides
//! strings in place (preserving the `String` JSON type) and
//! sentinel-replaces oversize arrays/objects only after recursion.
//!
//! Invariant: `kept_bytes < total_bytes` whenever `WalkOutcome::Elided`
//! is returned. The walker checks the post-walk size of `kept` against
//! the input's serialized size and bails to `Passthrough` if elision
//! didn't actually shrink — same precaution that fixes the line-count
//! overlap bug for plain-text inputs.

use serde_json::{Map, Value};

use super::{ElidedPath, ElisionKind};

/// Per-string head/tail char count when byte-eliding. Picked to keep
/// the elided form well under `DEFAULT_GENERIC_THRESHOLD_BYTES` so a
/// single oversize string field doesn't dominate its parent's budget.
const STRING_ELIDE_HEAD_CHARS: usize = 256;
const STRING_ELIDE_TAIL_CHARS: usize = 256;
const STRING_ELIDE_MARKER: &str = "…";

/// Per-array sentinel head/tail item count. Decremented on the way
/// down when even the trimmed sample exceeds the budget.
const ARRAY_SENTINEL_HEAD: usize = 3;
const ARRAY_SENTINEL_TAIL: usize = 3;

/// Soft cap for an emitted sentinel object's serialized size. The
/// walker drops head/tail items in the sentinel until it fits.
const SENTINEL_BYTE_BUDGET: usize = 2048;

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

pub fn classify_value(value: &Value, threshold: usize) -> WalkOutcome {
    let total_bytes = json_size(value);
    if total_bytes < threshold {
        return WalkOutcome::Passthrough(value.clone());
    }
    let mut paths = Vec::new();
    let kept = walk(value, threshold, "$".to_string(), &mut paths);
    let kept_bytes = json_size(&kept);
    if kept_bytes >= total_bytes {
        // Walk didn't help — bail.
        return WalkOutcome::Passthrough(value.clone());
    }
    WalkOutcome::Elided {
        kept,
        elided_paths: paths,
        total_bytes: total_bytes as u64,
        kept_bytes: kept_bytes.min(u32::MAX as usize) as u32,
    }
}

fn walk(value: &Value, threshold: usize, path: String, paths: &mut Vec<ElidedPath>) -> Value {
    let bytes = json_size(value);
    if bytes < threshold {
        return value.clone();
    }
    match value {
        Value::String(s) => byte_elide_string(s, &path, paths, bytes),
        Value::Array(items) => walk_array(items, threshold, &path, paths, bytes),
        Value::Object(map) => walk_object(map, threshold, &path, paths, bytes),
        // Numbers, booleans, null can't be over-threshold in practice.
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
    let elided = if head_end < tail_start {
        format!(
            "{}{}{}",
            &s[..head_end],
            STRING_ELIDE_MARKER,
            &s[tail_start..]
        )
    } else {
        // Head and tail would overlap; nothing to elide meaningfully.
        s.to_string()
    };
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
    path: &str,
    paths: &mut Vec<ElidedPath>,
    original_bytes: usize,
) -> Value {
    let walked: Vec<Value> = items
        .iter()
        .enumerate()
        .map(|(i, v)| walk(v, threshold, format!("{path}[{i}]"), paths))
        .collect();
    let walked_value = Value::Array(walked.clone());
    if json_size(&walked_value) < threshold {
        return walked_value;
    }
    sentinel_array(&walked, items.len(), path, paths, original_bytes)
}

fn sentinel_array(
    walked_items: &[Value],
    original_length: usize,
    path: &str,
    paths: &mut Vec<ElidedPath>,
    original_bytes: usize,
) -> Value {
    let mut head_count = ARRAY_SENTINEL_HEAD.min(walked_items.len());
    let max_tail = walked_items.len().saturating_sub(head_count);
    let mut tail_count = ARRAY_SENTINEL_TAIL.min(max_tail);
    let mut sentinel = make_array_sentinel(
        walked_items,
        head_count,
        tail_count,
        original_length,
        original_bytes,
    );
    while json_size(&sentinel) > SENTINEL_BYTE_BUDGET && (head_count > 0 || tail_count > 0) {
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
) -> Value {
    let head: Vec<Value> = walked_items.iter().take(head_count).cloned().collect();
    let tail: Vec<Value> = walked_items
        .iter()
        .rev()
        .take(tail_count)
        .rev()
        .cloned()
        .collect();
    serde_json::json!({
        "_elided": true,
        "kind": "array",
        "length": length,
        "bytes": bytes,
        "head": head,
        "tail": tail,
    })
}

fn walk_object(
    map: &Map<String, Value>,
    threshold: usize,
    path: &str,
    paths: &mut Vec<ElidedPath>,
    _original_bytes: usize,
) -> Value {
    let walked: Map<String, Value> = map
        .iter()
        .map(|(k, v)| (k.clone(), walk(v, threshold, format!("{path}.{k}"), paths)))
        .collect();
    let walked_value = Value::Object(walked.clone());
    if json_size(&walked_value) < threshold {
        return walked_value;
    }
    peel_object_keys(walked, threshold, path, paths)
}

fn peel_object_keys(
    mut walked: Map<String, Value>,
    threshold: usize,
    path: &str,
    paths: &mut Vec<ElidedPath>,
) -> Value {
    loop {
        let current_size = json_size(&Value::Object(walked.clone()));
        if current_size < threshold {
            break;
        }
        // Largest verbatim (non-sentinel) value wins the peel.
        let candidate = walked
            .iter()
            .filter(|(_, v)| !is_sentinel(v))
            .map(|(k, v)| (k.clone(), json_size(v)))
            .max_by_key(|(_, size)| *size);
        let Some((key, _)) = candidate else {
            // Nothing left to peel — give up; the parent's
            // post-walk size check will route us back to
            // Passthrough if we made things worse.
            break;
        };
        let value = walked
            .remove(&key)
            .expect("just selected this key from the same map");
        let bytes = json_size(&value);
        let (kind, length) = match &value {
            Value::String(_) => (ElisionKind::String, None),
            Value::Array(a) => (ElisionKind::Array, Some(a.len())),
            Value::Object(o) => (ElisionKind::Object, Some(o.len())),
            // Scalars can't be peeled into a useful sentinel; reinsert
            // and stop.
            _ => {
                walked.insert(key, value);
                break;
            }
        };
        paths.push(ElidedPath {
            path: format!("{path}.{key}"),
            kind,
            bytes: bytes as u64,
            length,
            preview: None,
        });
        walked.insert(key, sentinel_for(kind, bytes, length));
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

    /// Walker outputs `Elided` only when the elided form is strictly
    /// smaller than the raw input — load-bearing invariant for every
    /// downstream consumer of `kept_bytes`/`total_bytes`.
    #[test]
    fn walker_never_inflates_serialized_form() {
        let v = serde_json::json!((0..400).collect::<Vec<_>>());
        let raw_bytes = json_size(&v);
        let outcome = classify_value(&v, 4096);
        match outcome {
            WalkOutcome::Passthrough(passed) => assert_eq!(passed, v),
            WalkOutcome::Elided { kept_bytes, .. } => {
                assert!(
                    (kept_bytes as usize) < raw_bytes,
                    "kept_bytes ({kept_bytes}) must be strictly smaller \
                     than raw_bytes ({raw_bytes}) — invariant violated"
                );
            }
        }
    }

    /// Deeply-nested value: byte-elide the leaf string only; every
    /// ancestor object stays verbatim and is reachable by key.
    #[test]
    fn walker_preserves_structure_to_leaf() {
        let huge = "x".repeat(20_000);
        let v = serde_json::json!({
            "a": { "b": { "c": { "d": huge } } }
        });
        let outcome = classify_value(&v, 4096);
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
        assert!(leaf.len() < 20_000, "leaf must be byte-elided");
        assert_eq!(elided_paths.len(), 1);
        assert_eq!(elided_paths[0].path, "$.a.b.c.d");
        assert!(matches!(elided_paths[0].kind, ElisionKind::String));
    }

    /// Bash-shaped wrapper `{output: "<huge>"}`: wrapper object
    /// verbatim, inner string byte-elided in place. Critically, the
    /// `output` key is still a JSON string — scripts reading
    /// `result.output.split(...)` keep working.
    #[test]
    fn walker_string_elision_preserves_type() {
        let huge = "y".repeat(10_000);
        let v = serde_json::json!({ "output": huge });
        let outcome = classify_value(&v, 4096);
        let WalkOutcome::Elided { kept, .. } = outcome else {
            panic!("expected Elided");
        };
        let output = kept.get("output").expect("key present");
        assert!(
            output.is_string(),
            "output field must remain a JSON string after walking"
        );
        let s = output.as_str().unwrap();
        assert!(s.len() < 10_000, "string must shrink");
        assert!(
            s.contains(STRING_ELIDE_MARKER),
            "ellipsis marker present in elided string"
        );
    }

    /// Many small array items totalling over threshold: recursion
    /// can't shrink each item, walker emits a typed sentinel with
    /// head/tail samples.
    #[test]
    fn walker_emits_array_sentinel_when_items_irreducible() {
        let items: Vec<Value> = (0..200)
            .map(|i| Value::String(format!("item-{i:03}-{}", "z".repeat(20))))
            .collect();
        let v = Value::Array(items);
        let outcome = classify_value(&v, 4096);
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

    /// Object with one massively-large value: peel that key, others
    /// stay verbatim.
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
        let outcome = classify_value(&v, 4096);
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
        assert!(
            elided_paths.iter().any(|p| p.path.contains("huge_array")),
            "expected an elided path mentioning huge_array; got {:?}",
            elided_paths
                .iter()
                .map(|p| p.path.as_str())
                .collect::<Vec<_>>()
        );
    }

    /// `classify_value` short-circuits on small input, the walker
    /// itself is never called.
    #[test]
    fn walker_passthroughs_below_threshold() {
        let v = serde_json::json!({"hi": "world"});
        let outcome = classify_value(&v, 4096);
        match outcome {
            WalkOutcome::Passthrough(passed) => assert_eq!(passed, v),
            WalkOutcome::Elided { .. } => panic!("small input must Passthrough"),
        }
    }
}
