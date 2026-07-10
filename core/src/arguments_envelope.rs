//! Recovery for a recurring model failure mode: some models (GLM family
//! observed) emit a tool call's arguments wrapped one level too deep as
//! `{"arguments": {…}}` instead of `{…}`. The outer object then fails the
//! tool's schema, the error is fed back, and the model retries the same
//! wrong shape — with no natural exit, a workflow step can loop forever.
//!
//! The dispatch layers (`ToolSets::call_top_level_tool` and the
//! `call_tool` catalog for upstream MCP tools) call [`strip_for_dispatch`]
//! *after* a call fails to recover the intended arguments and retry once.
//! [`unwrap_recorded_input`] is the read-side mirror for the one place a
//! tool's raw input is later read back as a value (the `submit_output`
//! payload), so a recovered call still records the unwrapped shape.

use rmcp::model::JsonObject;
use serde_json::Value;

/// Inner object when `obj` is exactly a single-key `{"arguments": {…}}`
/// wrapper; `None` otherwise. Shared core of both entry points.
fn lone_arguments_inner(obj: &JsonObject) -> Option<&Value> {
    if obj.len() != 1 {
        return None;
    }
    let inner = obj.get("arguments")?;
    inner.as_object()?;
    Some(inner)
}

/// True when `schema` (a JSON Schema object) declares its own top-level
/// `arguments` property — such tools legitimately take an `arguments`
/// field and must never be unwrapped.
fn declares_arguments(schema: &Value) -> bool {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|props| props.contains_key("arguments"))
}

/// If `args` is a lone `{"arguments": {…}}` wrapper and `schema` does not
/// itself declare an `arguments` property, return the inner object so the
/// dispatch can retry with the shape the model meant. `None` leaves the
/// original call untouched — so a well-formed call is never second-guessed
/// and a tool that really wants `arguments` is never mangled.
pub(crate) fn strip_for_dispatch(args: &JsonObject, schema: &Value) -> Option<JsonObject> {
    let inner = lone_arguments_inner(args)?;
    if declares_arguments(schema) {
        return None;
    }
    inner.as_object().cloned()
}

/// Read-side mirror of [`strip_for_dispatch`] for a recorded tool-use
/// input read back as a value (no schema in hand — the `submit_output`
/// placeholder never declares `arguments`, matching the dispatch guard).
/// Returns the unwrapped inner object, or `value` unchanged.
pub(crate) fn unwrap_recorded_input(value: Value) -> Value {
    match value.as_object().and_then(lone_arguments_inner) {
        Some(inner) => inner.clone(),
        None => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> JsonObject {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn strips_lone_arguments_wrapper() {
        let args = obj(serde_json::json!({ "arguments": { "success": true, "verdict": "x" } }));
        let schema = serde_json::json!({ "type": "object" });
        let inner = strip_for_dispatch(&args, &schema).expect("unwraps");
        assert_eq!(inner.get("success"), Some(&Value::Bool(true)));
        assert_eq!(inner.len(), 2);
    }

    #[test]
    fn leaves_wellformed_args_untouched() {
        let args = obj(serde_json::json!({ "success": true, "verdict": "x" }));
        let schema = serde_json::json!({ "type": "object" });
        assert!(strip_for_dispatch(&args, &schema).is_none());
    }

    #[test]
    fn skips_tools_that_declare_arguments() {
        let args = obj(serde_json::json!({ "arguments": { "success": true } }));
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "arguments": { "type": "object" } }
        });
        assert!(strip_for_dispatch(&args, &schema).is_none());
    }

    #[test]
    fn requires_arguments_to_be_the_only_key() {
        let args = obj(serde_json::json!({ "arguments": { "success": true }, "extra": 1 }));
        let schema = serde_json::json!({ "type": "object" });
        assert!(strip_for_dispatch(&args, &schema).is_none());
    }

    #[test]
    fn requires_arguments_value_to_be_an_object() {
        let args = obj(serde_json::json!({ "arguments": "not-an-object" }));
        let schema = serde_json::json!({ "type": "object" });
        assert!(strip_for_dispatch(&args, &schema).is_none());
    }

    #[test]
    fn unwrap_recorded_input_unwraps_wrapper() {
        let wrapped = serde_json::json!({ "arguments": { "success": true } });
        assert_eq!(
            unwrap_recorded_input(wrapped),
            serde_json::json!({ "success": true })
        );
    }

    #[test]
    fn unwrap_recorded_input_passes_through_plain_value() {
        let plain = serde_json::json!({ "success": true, "verdict": "x" });
        assert_eq!(unwrap_recorded_input(plain.clone()), plain);
    }
}
