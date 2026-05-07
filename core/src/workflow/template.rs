//! `${{ … }}` template substitution for workflow steps.
//!
//! Two contexts are addressable:
//!
//! - `${{ trigger.X.Y }}` — fields of `WorkflowRun.trigger_context`.
//! - `${{ steps.<name>.outputs.X.Y }}` — prior-step `StepResult.output`
//!   (the namespace mirrors GitHub Actions' `steps.<id>.outputs.<name>`
//!   so authors familiar with one syntax read the other).
//!
//! Path syntax is dot-walk plus bracket indexing for arrays
//! (`items[0].name`); no filter / wildcard / function shape.
//!
//! Substitution semantics (memo `019e01a4`, §"what's new §2"):
//!
//! - Whole-string match — a JSON string whose trimmed value is
//!   exactly `${{ path }}` — splices the resolved JSON value (object,
//!   array, number, etc.) directly. This lets `payload: "${{
//!   steps.triage.outputs.args }}"` produce a JSON object payload
//!   rather than the stringified form.
//! - Embedded match — `${{ … }}` inside a longer string — interpolates:
//!   strings/booleans/numbers are coerced naturally; objects/arrays
//!   are JSON-encoded.
//! - Missing path — both modes resolve to `null` (whole-string) or
//!   empty string (embedded). Parse-time validation rejects refs
//!   that are structurally unreachable; runtime null is "the field
//!   was optional and absent."

use std::collections::HashMap;

use serde_json::Value;
use thiserror::Error;

const OPEN: &str = "${{";
const CLOSE: &str = "}}";

/// One parsed `${{ … }}` reference. Carries enough context for
/// parse-time validation and runtime resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateRef {
    /// Raw lexeme including delimiters, used for error messages.
    pub raw: String,
    pub path: Vec<PathSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSegment {
    Field(String),
    Index(usize),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TemplateError {
    #[error("template ref starting with `{0}` is unterminated; expected `}}}}`")]
    Unterminated(String),
    #[error("template ref `{0}`: empty path")]
    EmptyPath(String),
    #[error("template ref `{0}`: malformed segment near `{1}`")]
    MalformedSegment(String, String),
    #[error("template ref `{0}`: unknown context root `{1}` (expected `trigger` or `steps`)")]
    UnknownRoot(String, String),
    #[error("template ref `{0}`: `steps.<name>` requires the literal `outputs.` namespace next")]
    MissingOutputsNamespace(String),
}

/// Resolution context borrowed for one substitution pass.
pub struct TemplateContext<'a> {
    pub trigger: &'a Value,
    pub steps: &'a HashMap<String, Value>,
}

impl TemplateContext<'_> {
    /// `None` when the path doesn't exist at this point in time
    /// (parse-time validation should already have rejected refs
    /// that can never resolve, but optional fields surface here).
    pub fn resolve(&self, r: &TemplateRef) -> Option<Value> {
        let mut iter = r.path.iter();
        let root_name = match iter.next()? {
            PathSegment::Field(s) => s.as_str(),
            PathSegment::Index(_) => return None,
        };
        let mut cur = match root_name {
            "trigger" => self.trigger.clone(),
            "steps" => {
                let step_name = match iter.next()? {
                    PathSegment::Field(s) => s.as_str(),
                    PathSegment::Index(_) => return None,
                };
                match iter.next()? {
                    PathSegment::Field(s) if s == "outputs" => {}
                    _ => return None,
                }
                self.steps.get(step_name)?.clone()
            }
            _ => return None,
        };
        for seg in iter {
            cur = match (cur, seg) {
                (Value::Object(map), PathSegment::Field(k)) => map.get(k).cloned()?,
                (Value::Array(arr), PathSegment::Index(i)) => arr.get(*i).cloned()?,
                _ => return None,
            };
        }
        Some(cur)
    }
}

/// Parse the body of a `${{ … }}` reference (delimiters already
/// stripped). Whitespace is tolerated around segments.
pub fn parse_path(body: &str) -> Result<TemplateRef, TemplateError> {
    let raw = format!("{OPEN} {} {CLOSE}", body.trim());
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(TemplateError::EmptyPath(raw));
    }

    let mut segments = Vec::new();
    let mut cur = String::new();
    let mut chars = trimmed.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '.' => {
                if !cur.is_empty() {
                    segments.push(PathSegment::Field(std::mem::take(&mut cur)));
                }
            }
            '[' => {
                if !cur.is_empty() {
                    segments.push(PathSegment::Field(std::mem::take(&mut cur)));
                }
                let mut idx_str = String::new();
                let mut closed = false;
                for ic in chars.by_ref() {
                    if ic == ']' {
                        closed = true;
                        break;
                    }
                    idx_str.push(ic);
                }
                if !closed {
                    return Err(TemplateError::MalformedSegment(
                        raw.clone(),
                        format!("[{idx_str}"),
                    ));
                }
                let n: usize = idx_str
                    .trim()
                    .parse()
                    .map_err(|_| TemplateError::MalformedSegment(raw.clone(), idx_str))?;
                segments.push(PathSegment::Index(n));
            }
            c if c.is_alphanumeric() || c == '_' || c == '-' => cur.push(c),
            c if c.is_whitespace() => {}
            other => {
                return Err(TemplateError::MalformedSegment(raw, other.to_string()));
            }
        }
    }
    if !cur.is_empty() {
        segments.push(PathSegment::Field(cur));
    }

    if segments.is_empty() {
        return Err(TemplateError::EmptyPath(raw));
    }

    Ok(TemplateRef {
        raw,
        path: segments,
    })
}

/// Statically validate the root of a parsed ref. `steps.<n>.outputs.…`
/// must include the literal `outputs` namespace; trigger refs only
/// require the `trigger` root.
pub fn validate_root(r: &TemplateRef) -> Result<(), TemplateError> {
    let mut iter = r.path.iter();
    let root = iter
        .next()
        .ok_or_else(|| TemplateError::EmptyPath(r.raw.clone()))?;
    let root_name = match root {
        PathSegment::Field(s) => s.as_str(),
        PathSegment::Index(_) => {
            return Err(TemplateError::UnknownRoot(
                r.raw.clone(),
                "<index>".to_string(),
            ))
        }
    };
    match root_name {
        "trigger" => Ok(()),
        "steps" => {
            // steps.<name>.outputs.<...>
            let _name = iter
                .next()
                .ok_or_else(|| TemplateError::EmptyPath(r.raw.clone()))?;
            match iter.next() {
                Some(PathSegment::Field(s)) if s == "outputs" => Ok(()),
                _ => Err(TemplateError::MissingOutputsNamespace(r.raw.clone())),
            }
        }
        other => Err(TemplateError::UnknownRoot(r.raw.clone(), other.to_string())),
    }
}

/// Returns `Some(ref)` when the trimmed string is exactly one
/// `${{ … }}` lexeme — the whole-string splice case.
fn whole_string_ref(s: &str) -> Result<Option<TemplateRef>, TemplateError> {
    let trimmed = s.trim();
    if !trimmed.starts_with(OPEN) || !trimmed.ends_with(CLOSE) {
        return Ok(None);
    }
    let inner = &trimmed[OPEN.len()..trimmed.len() - CLOSE.len()];
    if inner.contains(OPEN) {
        // multiple expressions; not a whole-string splice
        return Ok(None);
    }
    Ok(Some(parse_path(inner)?))
}

fn coerce_embedded(v: Value, out: &mut String) {
    match v {
        Value::String(s) => out.push_str(&s),
        Value::Null => {}
        Value::Bool(b) => out.push_str(if b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        other => {
            out.push_str(&serde_json::to_string(&other).unwrap_or_default());
        }
    }
}

/// Walk a JSON tree and substitute every string leaf. Whole-string
/// `${{ x }}` matches splice; embedded matches stringify.
pub fn substitute_value(value: &Value, ctx: &TemplateContext) -> Result<Value, TemplateError> {
    match value {
        Value::String(s) => {
            if let Some(r) = whole_string_ref(s)? {
                return Ok(ctx.resolve(&r).unwrap_or(Value::Null));
            }
            Ok(Value::String(substitute_in_string(s, ctx)?))
        }
        Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for v in arr {
                out.push(substitute_value(v, ctx)?);
            }
            Ok(Value::Array(out))
        }
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), substitute_value(v, ctx)?);
            }
            Ok(Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

/// String-only substitution — used for skill bodies, where the
/// surrounding container is itself text. Non-scalar resolved values
/// are JSON-encoded.
pub fn substitute_in_string(s: &str, ctx: &TemplateContext) -> Result<String, TemplateError> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open_idx) = rest.find(OPEN) {
        out.push_str(&rest[..open_idx]);
        let after_open = &rest[open_idx + OPEN.len()..];
        let close_idx = after_open.find(CLOSE).ok_or_else(|| {
            TemplateError::Unterminated(format!(
                "{OPEN}{}",
                &after_open[..after_open.len().min(40)]
            ))
        })?;
        let body = &after_open[..close_idx];
        let r = parse_path(body)?;
        let resolved = ctx.resolve(&r).unwrap_or(Value::Null);
        coerce_embedded(resolved, &mut out);
        rest = &after_open[close_idx + CLOSE.len()..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Extract every `${{ … }}` reference from a JSON tree (string
/// leaves only). Used for parse-time validation; runtime
/// substitution does its own pass.
pub fn extract_refs_in_value(v: &Value) -> Result<Vec<TemplateRef>, TemplateError> {
    let mut acc = Vec::new();
    extract_refs_in_value_inner(v, &mut acc)?;
    Ok(acc)
}

fn extract_refs_in_value_inner(v: &Value, acc: &mut Vec<TemplateRef>) -> Result<(), TemplateError> {
    match v {
        Value::String(s) => acc.extend(extract_refs_in_string(s)?),
        Value::Array(arr) => {
            for v in arr {
                extract_refs_in_value_inner(v, acc)?;
            }
        }
        Value::Object(map) => {
            for v in map.values() {
                extract_refs_in_value_inner(v, acc)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn extract_refs_in_string(s: &str) -> Result<Vec<TemplateRef>, TemplateError> {
    let mut refs = Vec::new();
    let mut rest = s;
    while let Some(open_idx) = rest.find(OPEN) {
        let after_open = &rest[open_idx + OPEN.len()..];
        let close_idx = after_open.find(CLOSE).ok_or_else(|| {
            TemplateError::Unterminated(format!(
                "{OPEN}{}",
                &after_open[..after_open.len().min(40)]
            ))
        })?;
        let body = &after_open[..close_idx];
        refs.push(parse_path(body)?);
        rest = &after_open[close_idx + CLOSE.len()..];
    }
    Ok(refs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx_with(trigger: Value, steps: HashMap<String, Value>) -> (Value, HashMap<String, Value>) {
        (trigger, steps)
    }

    #[test]
    fn parse_simple_dot_path() {
        let r = parse_path(" trigger.payload.build ").unwrap();
        assert_eq!(
            r.path,
            vec![
                PathSegment::Field("trigger".into()),
                PathSegment::Field("payload".into()),
                PathSegment::Field("build".into()),
            ]
        );
    }

    #[test]
    fn parse_bracket_index() {
        let r = parse_path("steps.x.outputs.items[2].name").unwrap();
        assert_eq!(
            r.path,
            vec![
                PathSegment::Field("steps".into()),
                PathSegment::Field("x".into()),
                PathSegment::Field("outputs".into()),
                PathSegment::Field("items".into()),
                PathSegment::Index(2),
                PathSegment::Field("name".into()),
            ]
        );
    }

    #[test]
    fn parse_rejects_empty_path() {
        assert!(matches!(
            parse_path("   "),
            Err(TemplateError::EmptyPath(_))
        ));
    }

    #[test]
    fn parse_rejects_unclosed_bracket() {
        assert!(matches!(
            parse_path("a[3"),
            Err(TemplateError::MalformedSegment(_, _))
        ));
    }

    #[test]
    fn validate_root_accepts_trigger() {
        validate_root(&parse_path("trigger.x").unwrap()).unwrap();
    }

    #[test]
    fn validate_root_accepts_steps_outputs() {
        validate_root(&parse_path("steps.foo.outputs.bar").unwrap()).unwrap();
    }

    #[test]
    fn validate_root_rejects_unknown() {
        assert!(matches!(
            validate_root(&parse_path("env.HOME").unwrap()),
            Err(TemplateError::UnknownRoot(_, _))
        ));
    }

    #[test]
    fn validate_root_rejects_steps_without_outputs() {
        assert!(matches!(
            validate_root(&parse_path("steps.foo.bar").unwrap()),
            Err(TemplateError::MissingOutputsNamespace(_))
        ));
    }

    #[test]
    fn whole_string_splices_object() {
        let mut steps = HashMap::new();
        steps.insert("triage".into(), json!({ "args": { "build": 1234 } }));
        let (trigger, steps) = ctx_with(json!({}), steps);
        let ctx = TemplateContext {
            trigger: &trigger,
            steps: &steps,
        };
        let input = json!({ "payload": "${{ steps.triage.outputs.args }}" });
        let out = substitute_value(&input, &ctx).unwrap();
        assert_eq!(out, json!({ "payload": { "build": 1234 } }));
    }

    #[test]
    fn embedded_stringifies_scalars_naturally() {
        let trigger = json!({ "payload": { "build": 1234, "pipeline": "galoy-bank" } });
        let steps = HashMap::new();
        let ctx = TemplateContext {
            trigger: &trigger,
            steps: &steps,
        };
        let input = json!({
            "note": "Build ${{ trigger.payload.build }} failed in ${{ trigger.payload.pipeline }}."
        });
        let out = substitute_value(&input, &ctx).unwrap();
        assert_eq!(out, json!({ "note": "Build 1234 failed in galoy-bank." }));
    }

    #[test]
    fn embedded_json_encodes_non_scalars() {
        let trigger = json!({ "payload": { "list": [1, 2, 3] } });
        let steps = HashMap::new();
        let ctx = TemplateContext {
            trigger: &trigger,
            steps: &steps,
        };
        let s = substitute_in_string("ids=${{ trigger.payload.list }}", &ctx).unwrap();
        assert_eq!(s, "ids=[1,2,3]");
    }

    #[test]
    fn missing_path_resolves_to_null_in_splice() {
        let trigger = json!({ "payload": {} });
        let steps = HashMap::new();
        let ctx = TemplateContext {
            trigger: &trigger,
            steps: &steps,
        };
        let input = json!({ "x": "${{ trigger.payload.absent }}" });
        let out = substitute_value(&input, &ctx).unwrap();
        assert_eq!(out, json!({ "x": null }));
    }

    #[test]
    fn missing_path_resolves_to_empty_in_embed() {
        let trigger = json!({});
        let steps = HashMap::new();
        let ctx = TemplateContext {
            trigger: &trigger,
            steps: &steps,
        };
        assert_eq!(
            substitute_in_string("[${{ trigger.x }}]", &ctx).unwrap(),
            "[]"
        );
    }

    #[test]
    fn array_indexing_resolves() {
        let mut steps = HashMap::new();
        steps.insert(
            "s".into(),
            json!({ "items": [{ "name": "a" }, { "name": "b" }] }),
        );
        let trigger = json!({});
        let ctx = TemplateContext {
            trigger: &trigger,
            steps: &steps,
        };
        let s = substitute_in_string("${{ steps.s.outputs.items[1].name }}", &ctx).unwrap();
        assert_eq!(s, "b");
    }

    #[test]
    fn extract_refs_walks_nested_json() {
        let v = json!({
            "outer": "${{ trigger.a }}",
            "list": ["${{ trigger.b }}", "literal"],
            "nested": { "x": "literal ${{ trigger.c }} more" }
        });
        let refs = extract_refs_in_value(&v).unwrap();
        assert_eq!(refs.len(), 3);
        let raws: Vec<_> = refs.iter().map(|r| r.raw.clone()).collect();
        assert!(raws.iter().any(|r| r.contains("trigger.a")));
        assert!(raws.iter().any(|r| r.contains("trigger.b")));
        assert!(raws.iter().any(|r| r.contains("trigger.c")));
    }

    #[test]
    fn unterminated_template_errors() {
        let res = substitute_in_string(
            "build ${{ trigger.x is unterminated",
            &TemplateContext {
                trigger: &json!({}),
                steps: &HashMap::new(),
            },
        );
        assert!(matches!(res, Err(TemplateError::Unterminated(_))));
    }

    #[test]
    fn pure_literal_is_unchanged() {
        let trigger = json!({});
        let steps = HashMap::new();
        let ctx = TemplateContext {
            trigger: &trigger,
            steps: &steps,
        };
        let v = json!({ "k": "no templates here" });
        assert_eq!(substitute_value(&v, &ctx).unwrap(), v);
    }
}
