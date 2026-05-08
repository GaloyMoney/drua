//! `${{ … }}` template substitution for workflow steps.
//!
//! Two contexts are addressable:
//!
//! - `${{ trigger.X.Y }}` — fields of `WorkflowRun.trigger_context`.
//! - `${{ steps.<name>.outputs.X.Y }}` — prior-step `StepResult.output`
//!   (the namespace mirrors GitHub Actions' `steps.<id>.outputs.<name>`
//!   so authors familiar with one syntax read the other).
//!
//! The expression body inside the delimiters is plain CEL evaluated by
//! the `cel` crate (Common Expression Language — same family Swamp
//! and GHA use). The two namespace identifiers `trigger` and `steps`
//! are bound as variables on the evaluation context; everything else
//! the language offers (boolean operators, `size(…)`, `has(…)`,
//! `map`/`filter` macros, …) comes for free.
//!
//! Substitution semantics (memo `019e01a4`, §"what's new §2"):
//!
//! - Whole-string match — a JSON string whose trimmed value is
//!   exactly `${{ … }}` — splices the resolved JSON value (object,
//!   array, number, etc.) directly. Authors write
//!   `payload: "${{ steps.triage.outputs.args }}"` and get a JSON
//!   object payload, not the stringified form.
//! - Embedded match — `${{ … }}` inside a longer string —
//!   interpolates: strings/booleans/numbers coerce naturally,
//!   objects/arrays JSON-encode.
//! - Missing path / null result — both modes resolve to `null`
//!   (whole-string) or empty string (embedded). Parse-time
//!   validation rejects refs that are structurally unreachable;
//!   runtime null is "the field was optional and absent."

use std::collections::HashSet;
use std::sync::OnceLock;

use cel::{Context, Program};
use regex::Regex;
use serde_json::Value;
use thiserror::Error;

const OPEN: &str = "${{";
const CLOSE: &str = "}}";

/// One parsed `${{ … }}` reference. Holds the body text plus the
/// surrounding lexeme for error messages; the CEL program is
/// recompiled on demand to keep `TemplateRef` cheaply `Clone`-able
/// (`cel::Program` is not `Clone`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateRef {
    pub raw: String,
    pub body: String,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TemplateError {
    #[error("template ref starting with `{0}` is unterminated; expected `}}}}`")]
    Unterminated(String),
    #[error("template ref `{0}`: empty expression")]
    EmptyPath(String),
    #[error("template ref `{0}`: failed to compile CEL expression: {1}")]
    Compile(String, String),
    #[error("template ref `{0}`: references unknown identifier `{1}` (only `trigger` and `steps` are bound)")]
    UnknownRoot(String, String),
    #[error("template ref `{0}`: failed to evaluate at run time: {1}")]
    Resolve(String, String),
    #[error("template ref `{0}`: result could not be converted to JSON: {1}")]
    JsonConvert(String, String),
}

/// Resolution context borrowed for one substitution pass. The two
/// namespaces are bound as CEL variables of the matching name.
pub struct TemplateContext<'a> {
    pub trigger: &'a Value,
    pub steps: &'a std::collections::HashMap<String, Value>,
}

/// CEL context built from a [`TemplateContext`], cached so a single
/// `substitute_value` / `substitute_in_string` pass binds `trigger`
/// and `steps` exactly once instead of cloning the whole map per
/// reference. Held only for the duration of a substitution call —
/// no shared state across passes.
struct BuiltContext {
    cel: Context<'static>,
}

impl TemplateContext<'_> {
    fn build_cel_context(&self) -> Result<BuiltContext, TemplateError> {
        let mut ctx = Context::default();
        // GHA / Swamp convention: `${{ trigger.payload.X }}` for the
        // run's user payload, leaving room for future trigger
        // metadata (`trigger.received_at`, etc.) without breaking
        // existing references. The executor passes the raw payload;
        // wrapping happens here so callers don't have to.
        //
        // Null / non-object payloads (manual `trigger` with no
        // arguments, webhook with empty body, …) are coerced to an
        // empty object before binding. Without this, CEL evaluates
        // `null.field` to `Bool(false)` rather than raising
        // `NoSuchKey`, and the `false` ends up spliced into the
        // params tree — surprising the consuming tool with a bool
        // where it expected a string. The empty-object form lets
        // every `trigger.payload.X` reference resolve cleanly to
        // `null` via the existing missing-key path.
        let payload = match &self.trigger {
            Value::Object(_) => self.trigger.clone(),
            _ => Value::Object(serde_json::Map::new()),
        };
        let trigger_value = serde_json::json!({ "payload": payload });
        ctx.add_variable("trigger", trigger_value).map_err(|e| {
            TemplateError::Resolve("<context>".to_string(), format!("trigger: {e}"))
        })?;
        // Same convention for `${{ steps.<id>.outputs.<field> }}` —
        // wrap each step's recorded output under an `outputs` key so
        // the literal namespace traverses naturally as a CEL field
        // access.
        let steps_value = Value::Object(
            self.steps
                .iter()
                .map(|(k, v)| {
                    let mut wrapper = serde_json::Map::with_capacity(1);
                    wrapper.insert("outputs".to_string(), v.clone());
                    (k.clone(), Value::Object(wrapper))
                })
                .collect(),
        );
        ctx.add_variable("steps", steps_value)
            .map_err(|e| TemplateError::Resolve("<context>".to_string(), format!("steps: {e}")))?;
        Ok(BuiltContext { cel: ctx })
    }

    /// `None` when the path doesn't exist at this point in time
    /// (parse-time validation should already have rejected refs
    /// that can never resolve, but optional fields surface here).
    ///
    /// Convenience wrapper that builds the CEL context per call —
    /// fine for one-off resolutions; the substitution walks reuse
    /// a built context across all string leaves to amortise the
    /// `trigger` / `steps` clones.
    pub fn resolve(&self, r: &TemplateRef) -> Option<Value> {
        let built = self.build_cel_context().ok()?;
        resolve_with_built(&built, r)
    }
}

fn resolve_with_built(built: &BuiltContext, r: &TemplateRef) -> Option<Value> {
    let program = Program::compile(&r.body).ok()?;
    let value = program.execute(&built.cel).ok()?;
    // `value.json()` errors only on values CEL can produce that
    // JSON can't represent (functions, durations beyond i64
    // nanoseconds, …); not reachable for paths into JSON-typed
    // inputs, so silent-on-error is acceptable here.
    value.json().ok()
}

/// Parse the body of a `${{ … }}` reference (delimiters already
/// stripped). Compiles the CEL expression once to surface syntax
/// errors at parse time while keeping `TemplateRef` `Clone`-able.
pub fn parse_path(body: &str) -> Result<TemplateRef, TemplateError> {
    let trimmed = body.trim();
    let raw = format!("{OPEN} {trimmed} {CLOSE}");
    if trimmed.is_empty() {
        return Err(TemplateError::EmptyPath(raw));
    }
    Program::compile(trimmed).map_err(|e| TemplateError::Compile(raw.clone(), e.to_string()))?;
    Ok(TemplateRef {
        raw,
        body: trimmed.to_string(),
    })
}

/// Reject expressions that reference identifiers other than the two
/// bound namespaces (`trigger`, `steps`). Stops mistakes like
/// `${{ env.HOME }}` from compiling cleanly only to silently resolve
/// to nothing at runtime.
pub fn validate_root(r: &TemplateRef) -> Result<(), TemplateError> {
    let program = Program::compile(&r.body)
        .map_err(|e| TemplateError::Compile(r.raw.clone(), e.to_string()))?;
    for ident in program.references().variables() {
        if ident != "trigger" && ident != "steps" {
            return Err(TemplateError::UnknownRoot(r.raw.clone(), ident.to_string()));
        }
    }
    Ok(())
}

/// Names of every step referenced via `steps.<name>` in the
/// expression body. Used by parse-time forward-reference checking;
/// extracts via lexical scan rather than CEL AST walking (which would
/// require depending on the crate's internal `Expr` types). Ignores
/// bracket-style indexing (`steps["x"]`) — workflows must use
/// dot-syntax for static analyzability.
pub fn referenced_step_names(r: &TemplateRef) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\bsteps\.([A-Za-z_][A-Za-z0-9_-]*)").unwrap());
    let mut out: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for cap in re.captures_iter(&r.body) {
        let name = cap
            .get(1)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
        if seen.insert(name.clone()) {
            out.push(name);
        }
    }
    out
}

/// Returns `Some(ref)` when the trimmed string is exactly one
/// `${{ … }}` lexeme with no surrounding literal text — the
/// whole-string splice case. Inputs like
/// `"${{ x }} suffix"` (embedded with trailing literal) or
/// `"${{ a }}${{ b }}"` (multiple lexemes) return `None` and fall
/// through to embedded interpolation, which preserves the literal
/// text and stringifies each ref individually.
fn whole_string_ref(s: &str) -> Result<Option<TemplateRef>, TemplateError> {
    let trimmed = s.trim();
    if !trimmed.starts_with(OPEN) {
        return Ok(None);
    }
    let after_open = &trimmed[OPEN.len()..];
    // Find the first `}}`; if anything follows it (or another `${{`
    // appears inside the body), this isn't a whole-string splice.
    let close_idx = match after_open.find(CLOSE) {
        Some(i) => i,
        None => return Ok(None),
    };
    let body = &after_open[..close_idx];
    let after_close = &after_open[close_idx + CLOSE.len()..];
    if !after_close.is_empty() || body.contains(OPEN) {
        return Ok(None);
    }
    Ok(Some(parse_path(body)?))
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
    let built = ctx.build_cel_context()?;
    substitute_value_with_built(value, &built)
}

fn substitute_value_with_built(value: &Value, built: &BuiltContext) -> Result<Value, TemplateError> {
    match value {
        Value::String(s) => {
            if let Some(r) = whole_string_ref(s)? {
                return Ok(resolve_with_built(built, &r).unwrap_or(Value::Null));
            }
            Ok(Value::String(substitute_in_string_with_built(s, built)?))
        }
        Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for v in arr {
                out.push(substitute_value_with_built(v, built)?);
            }
            Ok(Value::Array(out))
        }
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), substitute_value_with_built(v, built)?);
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
    let built = ctx.build_cel_context()?;
    substitute_in_string_with_built(s, &built)
}

fn substitute_in_string_with_built(
    s: &str,
    built: &BuiltContext,
) -> Result<String, TemplateError> {
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
        let resolved = resolve_with_built(built, &r).unwrap_or(Value::Null);
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

/// One template substitution captured during a `substitute_value`
/// pass — used by callers that want to report which `${{ … }}`
/// reference produced which value when downstream consumers reject
/// the resolved tree.
#[derive(Debug, Clone)]
pub struct TemplateDiagnostic {
    /// JSON-pointer-style path into the params tree (e.g.
    /// `.tags[0]`, `.title`).
    pub json_path: String,
    /// The literal string from the original params, including the
    /// `${{ … }}` markers — useful when a single string carries
    /// multiple refs and surrounding literal text.
    pub source: String,
    /// The substituted value — what the consuming tool actually
    /// saw. Surfaces nulls, empty strings, and unexpected scalars
    /// in error messages.
    pub resolved: Value,
}

/// Walk the original and resolved params trees in parallel and
/// collect a diagnostic for every string leaf in `original` that
/// contained a `${{ … }}` reference. The list is the input/output
/// trace the executor includes when a tool dispatch rejects the
/// substituted params.
pub fn template_diagnostics(original: &Value, resolved: &Value) -> Vec<TemplateDiagnostic> {
    let mut out = Vec::new();
    walk_diagnostics(original, resolved, "", &mut out);
    out
}

fn walk_diagnostics(
    original: &Value,
    resolved: &Value,
    path: &str,
    acc: &mut Vec<TemplateDiagnostic>,
) {
    match (original, resolved) {
        (Value::String(s), r) if s.contains(OPEN) => {
            acc.push(TemplateDiagnostic {
                json_path: if path.is_empty() {
                    ".".to_string()
                } else {
                    path.to_string()
                },
                source: s.clone(),
                resolved: r.clone(),
            });
        }
        (Value::Object(o), Value::Object(r)) => {
            for (k, ov) in o {
                let next = format!("{path}.{k}");
                let rv = r.get(k).unwrap_or(&Value::Null);
                walk_diagnostics(ov, rv, &next, acc);
            }
        }
        (Value::Array(o), Value::Array(r)) => {
            for (i, ov) in o.iter().enumerate() {
                let next = format!("{path}[{i}]");
                let rv = r.get(i).unwrap_or(&Value::Null);
                walk_diagnostics(ov, rv, &next, acc);
            }
        }
        _ => {}
    }
}

/// Render a `Vec<TemplateDiagnostic>` as a human-readable multi-line
/// block suited for embedding in a `WorkflowError` reason. Empty
/// slices produce empty output.
pub fn format_template_diagnostics(diags: &[TemplateDiagnostic]) -> String {
    if diags.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n  template substitutions:\n");
    for d in diags {
        let resolved = serde_json::to_string(&d.resolved).unwrap_or_else(|_| "<?>".to_string());
        let source_escaped = d.source.replace('\n', "\\n");
        out.push_str(&format!(
            "    {}: {} → {}\n",
            d.json_path, source_escaped, resolved
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn ctx_with(trigger: Value, steps: HashMap<String, Value>) -> (Value, HashMap<String, Value>) {
        (trigger, steps)
    }

    #[test]
    fn parse_simple_dot_path() {
        let r = parse_path(" trigger.payload.build ").unwrap();
        assert_eq!(r.body, "trigger.payload.build");
    }

    #[test]
    fn parse_bracket_index() {
        let r = parse_path("steps.x.outputs.items[2].name").unwrap();
        assert_eq!(r.body, "steps.x.outputs.items[2].name");
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
            Err(TemplateError::Compile(_, _))
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
    fn validate_root_accepts_function_calls_over_bound_vars() {
        // `size`/`has` etc. are CEL standard library — they're
        // functions, not variables, so they don't count as new roots.
        validate_root(&parse_path("size(steps.x.outputs.list) > 0").unwrap()).unwrap();
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
        // CEL surfaces the inner map; we splice it as the JSON object
        // it represents. Field names round-trip; numeric types might
        // become integer-typed even if the input was an i64 literal.
        assert_eq!(
            out.get("payload")
                .and_then(|p| p.get("build"))
                .and_then(|b| b.as_i64()),
            Some(1234)
        );
    }

    #[test]
    fn embedded_stringifies_scalars_naturally() {
        // `TemplateContext.trigger` is the raw user payload; the
        // resolver wraps it under `payload` automatically so
        // `${{ trigger.payload.X }}` matches the GHA convention.
        let trigger = json!({ "build": 1234, "pipeline": "galoy-bank" });
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
        let trigger = json!({ "list": [1, 2, 3] });
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
        let trigger = json!({});
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
            substitute_in_string("[${{ trigger.payload.x }}]", &ctx).unwrap(),
            "[]"
        );
    }

    /// Run triggered with no payload — `trigger_context` arrives as
    /// JSON `null`. Templates referring into the (absent) payload
    /// must resolve silently, not produce a CEL error or splice a
    /// surprise scalar like `false`.
    #[test]
    fn null_payload_resolves_silently_in_splice() {
        let trigger = json!(null);
        let steps = HashMap::new();
        let ctx = TemplateContext {
            trigger: &trigger,
            steps: &steps,
        };
        let input = json!({ "x": "${{ trigger.payload.pipeline }}" });
        let out = substitute_value(&input, &ctx).unwrap();
        assert_eq!(out, json!({ "x": null }));
    }

    #[test]
    fn null_payload_resolves_silently_in_embed() {
        let trigger = json!(null);
        let steps = HashMap::new();
        let ctx = TemplateContext {
            trigger: &trigger,
            steps: &steps,
        };
        let s = substitute_in_string("build-${{ trigger.payload.build }}", &ctx).unwrap();
        assert_eq!(s, "build-");
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
        let bodies: Vec<_> = refs.iter().map(|r| r.body.clone()).collect();
        assert!(bodies.iter().any(|b| b == "trigger.a"));
        assert!(bodies.iter().any(|b| b == "trigger.b"));
        assert!(bodies.iter().any(|b| b == "trigger.c"));
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

    /// Trigger payload data is passed in opaquely (it may carry
    /// user-controlled text from a webhook). A literal `${{ … }}`
    /// in the payload must NOT be re-evaluated when it lands in
    /// substituted output — otherwise the consumer would gain a
    /// path to leak prior step outputs by reflecting them through
    /// `${{ steps.X.outputs.Y }}` smuggled through the trigger.
    ///
    /// `substitute_in_string` makes one pass over its input; the
    /// resolved values are spliced verbatim. This test pins that
    /// behaviour: the literal `${{ steps.victim.outputs.secret }}`
    /// inside `trigger.payload.commit_msg` survives substitution
    /// as text without being interpreted as a template ref.
    #[test]
    fn substitute_does_not_re_evaluate_literals_from_resolved_values() {
        let trigger = json!({
            "commit_msg": "Fix typo. Side note: ${{ steps.victim.outputs.secret }}"
        });
        let mut steps = HashMap::new();
        steps.insert("victim".into(), json!({ "secret": "hunter2" }));
        let ctx = TemplateContext {
            trigger: &trigger,
            steps: &steps,
        };
        let out = substitute_in_string("commit: ${{ trigger.payload.commit_msg }}", &ctx).unwrap();
        // Literal `${{ steps.victim.outputs.secret }}` survives as
        // text. `hunter2` does NOT appear.
        assert!(out.contains("${{ steps.victim.outputs.secret }}"));
        assert!(!out.contains("hunter2"));
    }

    /// `"${{ x }} suffix }}"` ends with `}}` and has no second
    /// `${{` — a naive whole-string check would treat it as a splice,
    /// then try to compile `x }} suffix` as a CEL expression and
    /// return an unhelpful compile error. Detection must require an
    /// EMPTY tail after the first `}}` to count as whole-string.
    #[test]
    fn embedded_match_with_trailing_close_chars_is_not_a_whole_string_splice() {
        let trigger = json!({ "x": "X" });
        let steps = HashMap::new();
        let ctx = TemplateContext {
            trigger: &trigger,
            steps: &steps,
        };
        let input = json!({ "v": "${{ trigger.payload.x }} suffix }}" });
        let out = substitute_value(&input, &ctx).unwrap();
        // Embedded interpolation: the `${{ … }}` resolves to "X",
        // the trailing literal " suffix }}" is preserved verbatim.
        assert_eq!(out, json!({ "v": "X suffix }}" }));
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

    #[test]
    fn referenced_step_names_extracts_unique_ordered() {
        let r = parse_path(
            "steps.triage.outputs.x + steps.dispatch.outputs.y + steps.triage.outputs.z",
        )
        .unwrap();
        let names = referenced_step_names(&r);
        assert_eq!(names, vec!["triage".to_string(), "dispatch".to_string()]);
    }

    #[test]
    fn template_diagnostics_records_path_source_and_resolved() {
        let trigger = json!(null);
        let mut steps = HashMap::new();
        steps.insert(
            "identify".into(),
            json!({ "workflow_run_id": "abc-123", "type": "workflow_executor" }),
        );
        let ctx = TemplateContext {
            trigger: &trigger,
            steps: &steps,
        };
        let original = json!({
            "title": "run-${{ steps.identify.outputs.workflow_run_id }}",
            "tags": [
                "${{ trigger.payload.pipeline }}",
                "build-${{ trigger.payload.build }}"
            ]
        });
        let resolved = substitute_value(&original, &ctx).unwrap();
        let diags = template_diagnostics(&original, &resolved);
        assert_eq!(diags.len(), 3);

        let title = diags.iter().find(|d| d.json_path == ".title").unwrap();
        assert_eq!(title.resolved, json!("run-abc-123"));

        let pipeline = diags.iter().find(|d| d.json_path == ".tags[0]").unwrap();
        assert!(pipeline.source.contains("trigger.payload.pipeline"));
        assert_eq!(pipeline.resolved, Value::Null);

        let build = diags.iter().find(|d| d.json_path == ".tags[1]").unwrap();
        assert_eq!(build.resolved, json!("build-"));

        let formatted = format_template_diagnostics(&diags);
        assert!(formatted.contains(".title"));
        assert!(formatted.contains(".tags[0]"));
        assert!(formatted.contains("trigger.payload.pipeline"));
        assert!(formatted.contains("null"));
    }

    #[test]
    fn referenced_step_names_ignores_trigger() {
        let r = parse_path("trigger.payload.build").unwrap();
        assert!(referenced_step_names(&r).is_empty());
    }
}
