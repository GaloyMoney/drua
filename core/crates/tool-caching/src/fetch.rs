//! Recovery side of the universal pipeline: given a stored invocation,
//! navigate a json-path and optionally slice the resolved value.

use rmcp::model::{CallToolResult, Content};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ToolCachingError;
use crate::repo::StoredInvocation;

/// Slice operation applied at the resolved json-path. When absent, the
/// whole value at the path is returned. Mirrors the recovery template
/// the walker emits.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum FetchQuery {
    /// Byte range on a string-typed value at `path`.
    Range { offset: usize, len: usize },
    /// Line range on a string-typed value at `path`. `offset` is the
    /// zero-indexed first line to return; `len` is the line count.
    Lines { offset: usize, len: usize },
    /// Item range on an array-typed value at `path`. Returns the slice
    /// `arr[offset..offset+len]` as a `Value::Array`.
    JsonArraySlice { offset: usize, len: usize },
}

impl FetchQuery {
    fn apply(&self, value: Value) -> Result<Value, ToolCachingError> {
        match self {
            FetchQuery::Range { offset, len } => {
                let s = value.as_str().ok_or_else(|| {
                    ToolCachingError::InvalidPath(
                        "range query requires a string at the resolved path".into(),
                    )
                })?;
                let end = offset.saturating_add(*len).min(s.len());
                let start = (*offset).min(s.len());
                let slice = s.get(start..end).ok_or_else(|| {
                    ToolCachingError::InvalidPath("range cuts a non-char-boundary".into())
                })?;
                Ok(Value::String(slice.to_string()))
            }
            FetchQuery::Lines { offset, len } => {
                let s = value.as_str().ok_or_else(|| {
                    ToolCachingError::InvalidPath(
                        "lines query requires a string at the resolved path".into(),
                    )
                })?;
                let all: Vec<&str> = s.lines().collect();
                let start = (*offset).min(all.len());
                let end = offset.saturating_add(*len).min(all.len());
                Ok(Value::String(all[start..end].join("\n")))
            }
            FetchQuery::JsonArraySlice { offset, len } => {
                let arr = value.as_array().ok_or_else(|| {
                    ToolCachingError::InvalidPath(
                        "json_array_slice query requires an array at the resolved path".into(),
                    )
                })?;
                let start = (*offset).min(arr.len());
                let end = offset.saturating_add(*len).min(arr.len());
                Ok(Value::Array(arr[start..end].to_vec()))
            }
        }
    }
}

/// What `ToolCaching::fetch` hands back to the caller. `result` is the
/// agent-facing `CallToolResult`; `structured` is the wrapped json
/// value at `path` (e.g. `{"a": {"b": <slice>}}` for `path="$.a.b"`),
/// surfaced separately so the dispatch layer / compose can consume it
/// without re-parsing the text channel.
pub struct FetchResult {
    pub result: CallToolResult,
    pub structured: Value,
}

impl StoredInvocation {
    /// Resolve `path` against the stored root and slice with `query`.
    /// Wraps the result back at `path` so the response shape mirrors
    /// the caller's request (`$.foo[2]` → `{"foo": [X]}`). Responses
    /// over `max_bytes` (text channel size) are rejected.
    pub fn query(
        &self,
        path: &str,
        query: Option<&FetchQuery>,
        max_bytes: usize,
    ) -> Result<FetchResult, ToolCachingError> {
        let resolved = self.navigate(path)?.clone();
        let sliced = match query {
            Some(q) => q.apply(resolved)?,
            None => resolved,
        };
        let wrapped = Self::wrap_at_path(path, sliced)?;
        let text = match &wrapped {
            Value::String(s) => s.clone(),
            other => serde_json::to_string(other).unwrap_or_default(),
        };
        if text.len() > max_bytes {
            return Err(ToolCachingError::FetchResponseTooLarge {
                size: text.len(),
                max: max_bytes,
            });
        }
        Ok(FetchResult {
            result: CallToolResult::success(vec![Content::text(text)]),
            structured: wrapped,
        })
    }

    fn navigate(&self, path: &str) -> Result<&Value, ToolCachingError> {
        let segments = Self::parse_path(path)?;
        let mut cur = &self.query_structure.root;
        for seg in &segments {
            cur = match seg {
                PathSegment::Key(k) => cur.get(k).ok_or_else(|| {
                    ToolCachingError::InvalidPath(format!("path {path}: no `{k}`"))
                })?,
                PathSegment::Index(i) => cur.get(*i).ok_or_else(|| {
                    ToolCachingError::InvalidPath(format!("path {path}: no [{i}]"))
                })?,
            };
        }
        Ok(cur)
    }

    /// Rebuild the structure implied by `path` around `value`. For
    /// `path == "$"` returns `value` directly. Object-key segments nest
    /// into `{key: …}`; array-index segments wrap into a single-element
    /// array (`$[3]` → `[value]`) — callers can read `result[0]` and
    /// recover the original index from the recovery template's `path`
    /// field. Leading `null` padding scales linearly with the index and
    /// would burn tokens at every higher position without adding info.
    fn wrap_at_path(path: &str, value: Value) -> Result<Value, ToolCachingError> {
        let segments = Self::parse_path(path)?;
        let mut acc = value;
        for seg in segments.into_iter().rev() {
            acc = match seg {
                PathSegment::Key(k) => {
                    let mut obj = serde_json::Map::new();
                    obj.insert(k, acc);
                    Value::Object(obj)
                }
                PathSegment::Index(_) => Value::Array(vec![acc]),
            };
        }
        Ok(acc)
    }

    /// Tokenize a `$`-prefixed json-path into key / index segments.
    /// Matches the walker's emitted shape: dotted object keys, bracketed
    /// numeric array indices, and any mix of the two (`$.items[3].name`).
    fn parse_path(path: &str) -> Result<Vec<PathSegment>, ToolCachingError> {
        let Some(rest) = path.strip_prefix('$') else {
            return Err(ToolCachingError::InvalidPath(format!(
                "path must start with `$`; got {path}"
            )));
        };
        let bytes = rest.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'.' => {
                    i += 1;
                    let start = i;
                    while i < bytes.len() && bytes[i] != b'.' && bytes[i] != b'[' {
                        i += 1;
                    }
                    if start == i {
                        return Err(ToolCachingError::InvalidPath(format!(
                            "empty key segment in path {path}"
                        )));
                    }
                    out.push(PathSegment::Key(rest[start..i].to_string()));
                }
                b'[' => {
                    i += 1;
                    let start = i;
                    while i < bytes.len() && bytes[i] != b']' {
                        i += 1;
                    }
                    if i == bytes.len() {
                        return Err(ToolCachingError::InvalidPath(format!(
                            "unclosed `[` in path {path}"
                        )));
                    }
                    let idx: usize = rest[start..i].parse().map_err(|_| {
                        ToolCachingError::InvalidPath(format!("invalid array index in path {path}"))
                    })?;
                    out.push(PathSegment::Index(idx));
                    i += 1;
                }
                _ => {
                    return Err(ToolCachingError::InvalidPath(format!(
                        "path after `$` must be `.key` or `[index]`; got {path}"
                    )));
                }
            }
        }
        Ok(out)
    }
}

enum PathSegment {
    Key(String),
    Index(usize),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primitives::{QueryStructure, ToolCallSummary, ToolInvocationId};

    fn stored(root: Value) -> StoredInvocation {
        StoredInvocation {
            id: ToolInvocationId::new(),
            query_structure: QueryStructure { root },
            summary: ToolCallSummary {
                summary: Value::Null,
                elided_paths: Vec::new(),
                root_path: "$".to_string(),
                original_bytes: 0,
            },
            original_structured: None,
        }
    }

    #[test]
    fn wrap_at_path_root_is_identity() {
        let v = Value::String("hi".into());
        assert_eq!(StoredInvocation::wrap_at_path("$", v.clone()).unwrap(), v,);
    }

    #[test]
    fn wrap_at_path_nested_keys() {
        let wrapped = StoredInvocation::wrap_at_path("$.a.b", Value::String("hi".into())).unwrap();
        assert_eq!(wrapped, serde_json::json!({"a": {"b": "hi"}}));
    }

    #[test]
    fn wrap_at_path_array_root_uses_single_element() {
        let wrapped = StoredInvocation::wrap_at_path("$[2]", Value::String("hi".into())).unwrap();
        assert_eq!(wrapped, serde_json::json!(["hi"]));
    }

    #[test]
    fn wrap_at_path_mixed_keys_and_indices() {
        let wrapped =
            StoredInvocation::wrap_at_path("$.items[1].name", Value::String("hi".into())).unwrap();
        assert_eq!(wrapped, serde_json::json!({"items": [{"name": "hi"}]}),);
    }

    #[test]
    fn parse_path_handles_mixed_keys_and_indices() {
        let segs = StoredInvocation::parse_path("$.items[3].name").unwrap();
        assert_eq!(segs.len(), 3);
        assert!(matches!(&segs[0], PathSegment::Key(k) if k == "items"));
        assert!(matches!(&segs[1], PathSegment::Index(3)));
        assert!(matches!(&segs[2], PathSegment::Key(k) if k == "name"));
    }

    #[test]
    fn navigate_walks_into_nested_array() {
        let inv = stored(serde_json::json!({"items": ["a", "b", "c"]}));
        assert_eq!(
            inv.navigate("$.items[1]").unwrap(),
            &Value::String("b".into())
        );
    }

    #[test]
    fn fetch_query_range_slices_string() {
        let q = FetchQuery::Range { offset: 3, len: 4 };
        assert_eq!(
            q.apply(Value::String("0123456789".into())).unwrap(),
            Value::String("3456".into()),
        );
    }
}
