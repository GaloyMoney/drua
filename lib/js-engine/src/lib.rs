//! Lightweight JS engine wrapper around rquickjs for composing MCP tool calls.
//!
//! Provides a sandboxed JavaScript execution environment with async tool
//! dispatch via the [`ToolDispatcher`] trait. Each execution gets a fresh
//! runtime (no state carryover) with configurable resource limits.

mod error;

pub use error::JsEngineError;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rquickjs::function::Rest;
use rquickjs::prelude::Promised;
use rquickjs::{async_with, AsyncContext, AsyncRuntime, CatchResultExt, Function, Object};

/// Trait for dispatching tool calls from JS to the host.
#[async_trait::async_trait]
pub trait ToolDispatcher: Send + Sync + 'static {
    async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String>;
}

/// Result of executing a JS script.
#[derive(Debug)]
pub struct ExecutionResult {
    pub value: serde_json::Value,
    pub console_output: Vec<String>,
    pub tool_calls_made: usize,
    pub execution_time: Duration,
}

/// Lightweight JS engine wrapper around rquickjs.
///
/// Creates a fresh `AsyncRuntime` + `AsyncContext` per execution with
/// configurable memory, stack, and tool-call limits. The runtime is
/// dropped after each execution — no state carries over.
pub struct JsEngine {
    memory_limit: usize,
    stack_limit: usize,
    max_tool_calls: usize,
    max_result_bytes: usize,
}

impl Default for JsEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl JsEngine {
    pub fn new() -> Self {
        Self {
            memory_limit: 8 * 1024 * 1024, // 8 MB
            stack_limit: 512 * 1024,       // 512 KB
            max_tool_calls: 50,
            max_result_bytes: 100 * 1024, // 100 KB
        }
    }

    pub fn with_max_tool_calls(mut self, n: usize) -> Self {
        self.max_tool_calls = n;
        self
    }

    pub fn with_max_result_bytes(mut self, n: usize) -> Self {
        self.max_result_bytes = n;
        self
    }

    pub fn with_memory_limit(mut self, n: usize) -> Self {
        self.memory_limit = n;
        self
    }

    pub fn with_stack_limit(mut self, n: usize) -> Self {
        self.stack_limit = n;
        self
    }

    #[tracing::instrument(name = "js_engine.execute", skip_all)]
    pub async fn execute(
        &self,
        script: &str,
        dispatcher: Arc<dyn ToolDispatcher>,
        timeout: Duration,
    ) -> Result<ExecutionResult, JsEngineError> {
        let start = Instant::now();
        let max_tool_calls = self.max_tool_calls;
        let max_result_bytes = self.max_result_bytes;
        let memory_limit = self.memory_limit;
        let stack_limit = self.stack_limit;

        // Shared state between Rust host and JS guest
        let console_buf: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let tool_call_count = Arc::new(AtomicUsize::new(0));
        let timed_out = Arc::new(AtomicBool::new(false));

        let result = tokio::time::timeout(timeout, async {
            let rt = AsyncRuntime::new().map_err(|e| JsEngineError::Runtime(e.to_string()))?;
            rt.set_memory_limit(memory_limit).await;
            rt.set_max_stack_size(stack_limit).await;
            rt.set_gc_threshold(memory_limit / 2).await;

            // Interrupt handler: fires periodically during bytecode execution.
            // Returning true raises an uncatchable exception (defeats infinite loops).
            let deadline = start + timeout;
            let timed_out_flag = Arc::clone(&timed_out);
            rt.set_interrupt_handler(Some(Box::new(move || {
                if Instant::now() > deadline {
                    timed_out_flag.store(true, Ordering::Relaxed);
                    true
                } else {
                    false
                }
            })))
            .await;

            let ctx = AsyncContext::full(&rt)
                .await
                .map_err(|e| JsEngineError::Runtime(e.to_string()))?;

            let script = script.to_string();
            let console_buf_inner = Arc::clone(&console_buf);
            let tool_call_count_inner = Arc::clone(&tool_call_count);

            let value_json: String = async_with!(ctx => |ctx| {
                // ── Register console ────────────────────────────────────
                register_console(&ctx, Arc::clone(&console_buf_inner))
                    .map_err(|e| JsEngineError::Runtime(format!("console registration: {e}")))?;

                // ── Register tool bridge ────────────────────────────────
                register_tool_bridge(
                    &ctx,
                    Arc::clone(&dispatcher),
                    Arc::clone(&tool_call_count_inner),
                    max_tool_calls,
                    max_result_bytes,
                )
                .map_err(|e| JsEngineError::Runtime(format!("tool bridge registration: {e}")))?;

                // ── Eval bootstrap + wrapped user script ────────────────
                let full_script = build_full_script(&script);

                // Eval the script. The async IIFE returns a Promise; MaybePromise
                // handles both promise and non-promise results transparently.
                let maybe_promise: rquickjs::promise::MaybePromise<'_> =
                    match ctx.eval(full_script).catch(&ctx) {
                        Ok(v) => v,
                        Err(caught) => {
                            let msg = format_caught_error(&caught);
                            // The interrupt handler raises "interrupted" when
                            // the timeout fires during synchronous execution.
                            if msg.contains("interrupted") {
                                return Err(JsEngineError::Timeout(timeout));
                            }
                            return Err(JsEngineError::ScriptSyntax(msg));
                        }
                    };

                // Await if it's a promise (it always is due to async IIFE)
                let final_val: rquickjs::Value<'_> =
                    match maybe_promise.into_future().await.catch(&ctx) {
                        Ok(v) => v,
                        Err(caught) => {
                            let msg = format_caught_error(&caught);
                            if msg.contains("interrupted") {
                                return Err(JsEngineError::Timeout(timeout));
                            }
                            return Err(JsEngineError::ScriptRuntime(msg));
                        }
                    };

                // Serialize the result to JSON string inside the JS context
                let json_stringify: Function = ctx
                    .globals()
                    .get::<_, Object>("JSON")
                    .map_err(|e| JsEngineError::Runtime(format!("JSON global: {e}")))?
                    .get("stringify")
                    .map_err(|e| JsEngineError::Runtime(format!("JSON.stringify: {e}")))?;

                let json_str: String = json_stringify
                    .call((final_val,))
                    .unwrap_or_else(|_| "null".to_string());

                Ok(json_str)
            })
            .await?;

            // Drive any remaining pending jobs
            rt.idle().await;

            Ok::<String, JsEngineError>(value_json)
        })
        .await;

        // Handle timeout
        let value_json = match result {
            Ok(inner) => inner?,
            Err(_elapsed) => return Err(JsEngineError::Timeout(timeout)),
        };

        // Check interrupt-based timeout
        if timed_out.load(Ordering::Relaxed) {
            return Err(JsEngineError::Timeout(timeout));
        }

        // Parse the JSON result
        let value: serde_json::Value =
            serde_json::from_str(&value_json).unwrap_or(serde_json::Value::Null);

        // Check result size
        let result_size = value_json.len();
        if result_size > max_result_bytes {
            return Err(JsEngineError::ResultTooLarge {
                size: result_size,
                max: max_result_bytes,
            });
        }

        let console_output = console_buf.lock().unwrap().clone();
        let tool_calls_made = tool_call_count.load(Ordering::Relaxed);

        Ok(ExecutionResult {
            value,
            console_output,
            tool_calls_made,
            execution_time: start.elapsed(),
        })
    }
}

// ─── Internal helpers ────────────────────────────────────────────────────────

/// Register `console.log`, `console.warn`, `console.error`, `console.info`
/// as native functions that capture output to a shared buffer.
fn register_console(
    ctx: &rquickjs::Ctx<'_>,
    buf: Arc<std::sync::Mutex<Vec<String>>>,
) -> Result<(), rquickjs::Error> {
    let console = Object::new(ctx.clone())?;

    let b = Arc::clone(&buf);
    console.set(
        "log",
        Function::new(ctx.clone(), move |args: Rest<String>| {
            let msg = args.0.join(" ");
            b.lock().unwrap().push(msg);
        })?,
    )?;

    let b = Arc::clone(&buf);
    console.set(
        "info",
        Function::new(ctx.clone(), move |args: Rest<String>| {
            let msg = args.0.join(" ");
            b.lock().unwrap().push(msg);
        })?,
    )?;

    let b = Arc::clone(&buf);
    console.set(
        "warn",
        Function::new(ctx.clone(), move |args: Rest<String>| {
            let msg = format!("[WARN] {}", args.0.join(" "));
            b.lock().unwrap().push(msg);
        })?,
    )?;

    let b = Arc::clone(&buf);
    console.set(
        "error",
        Function::new(ctx.clone(), move |args: Rest<String>| {
            let msg = format!("[ERROR] {}", args.0.join(" "));
            b.lock().unwrap().push(msg);
        })?,
    )?;

    ctx.globals().set("console", console)?;
    Ok(())
}

/// Register `__call_tool_raw(name, args_json)` → Promise<string> as the
/// native bridge between JS and the host's [`ToolDispatcher`].
///
/// Returns a JSON-encoded envelope: `{"ok":true,"value":...}` or
/// `{"ok":false,"error":"..."}`. The JS bootstrap code unwraps this.
fn register_tool_bridge(
    ctx: &rquickjs::Ctx<'_>,
    dispatcher: Arc<dyn ToolDispatcher>,
    tool_call_count: Arc<AtomicUsize>,
    max_tool_calls: usize,
    max_result_bytes: usize,
) -> Result<(), rquickjs::Error> {
    ctx.globals().set(
        "__call_tool_raw",
        Function::new(ctx.clone(), move |name: String, args_json: String| {
            let d = Arc::clone(&dispatcher);
            let tc = Arc::clone(&tool_call_count);
            Promised(async move {
                // Enforce tool-call limit
                let count = tc.fetch_add(1, Ordering::Relaxed);
                if count >= max_tool_calls {
                    return encode_error(&format!(
                        "Tool call limit exceeded ({max_tool_calls} max)"
                    ));
                }

                // Parse args
                let args: serde_json::Value = match serde_json::from_str(&args_json) {
                    Ok(v) => v,
                    Err(e) => return encode_error(&format!("Invalid arguments JSON: {e}")),
                };

                // Dispatch
                match d.call_tool(&name, args).await {
                    Ok(result) => {
                        let result_json =
                            serde_json::to_string(&result).unwrap_or_else(|_| "null".into());
                        if result_json.len() > max_result_bytes {
                            return encode_error(&format!(
                                "Tool result too large ({} bytes, max {max_result_bytes})",
                                result_json.len()
                            ));
                        }
                        format!(r#"{{"ok":true,"value":{result_json}}}"#)
                    }
                    Err(e) => encode_error(&e),
                }
            })
        })?,
    )?;
    Ok(())
}

fn encode_error(msg: &str) -> String {
    let escaped = msg.replace('\\', "\\\\").replace('"', "\\\"");
    format!(r#"{{"ok":false,"error":"{escaped}"}}"#)
}

/// Build the full script: bootstrap (tools proxy) + user code wrapped in an
/// async IIFE for top-level await + return support.
///
/// The tools proxy supports two calling conventions:
/// - Flat: `tools.prefixed_name(args)` → `__call_tool_raw("prefixed_name", ...)`
/// - Nested: `tools.server.toolName(args)` → `__call_tool_raw("server_toolName", ...)`
///
/// The nested proxy is implemented via a two-level Proxy chain. The outer
/// proxy intercepts property access and returns an inner proxy per server
/// namespace. The inner proxy intercepts tool calls and prepends the server
/// prefix. If the outer property is called directly as a function, it falls
/// back to flat dispatch.
fn build_full_script(user_script: &str) -> String {
    format!(
        r#"// ── tool dispatch helper ─────────────────────────────
async function __dispatch(prefixedName, args) {{
    const raw = await __call_tool_raw(prefixedName, JSON.stringify(args || {{}}));
    const r = JSON.parse(raw);
    if (!r.ok) {{
        const err = new Error(r.error);
        err.isToolError = true;
        throw err;
    }}
    return r.value;
}}

// ── tools proxy (nested + flat) ─────────────────────
const tools = new Proxy({{}}, {{
    get(_, serverOrPrefixed) {{
        // Return a namespace proxy: tools.server.toolName(args)
        // If called directly as tools.prefixed_name(args), the namespace
        // proxy's apply trap handles it (via flat dispatch).
        const nsProxy = new Proxy(
            (args) => __dispatch(serverOrPrefixed, args),
            {{
                get(_, toolName) {{
                    return (args) => __dispatch(serverOrPrefixed + "_" + toolName, args);
                }},
                apply(target, thisArg, argsList) {{
                    return target(argsList[0]);
                }}
            }}
        );
        return nsProxy;
    }}
}});

// ── user script (wrapped for top-level await + return) ──
(async () => {{
{user_script}
}})()
"#
    )
}

fn format_caught_error(caught: &rquickjs::CaughtError<'_>) -> String {
    match caught {
        rquickjs::CaughtError::Exception(exc) => format!("{exc}"),
        rquickjs::CaughtError::Value(val) => format!("Thrown value: {val:?}"),
        rquickjs::CaughtError::Error(e) => format!("{e}"),
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoDispatcher;

    #[async_trait::async_trait]
    impl ToolDispatcher for EchoDispatcher {
        async fn call_tool(
            &self,
            name: &str,
            args: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({ "tool": name, "args": args }))
        }
    }

    struct FailingDispatcher;

    #[async_trait::async_trait]
    impl ToolDispatcher for FailingDispatcher {
        async fn call_tool(
            &self,
            _name: &str,
            _args: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            Err("tool call failed".to_string())
        }
    }

    fn engine() -> JsEngine {
        JsEngine::new()
    }

    fn timeout() -> Duration {
        Duration::from_secs(10)
    }

    #[tokio::test]
    async fn basic_eval_returns_value() {
        let result = engine()
            .execute("return 1 + 2;", Arc::new(EchoDispatcher), timeout())
            .await
            .unwrap();
        assert_eq!(result.value, serde_json::json!(3));
        assert_eq!(result.tool_calls_made, 0);
    }

    #[tokio::test]
    async fn eval_string_value() {
        let result = engine()
            .execute(
                r#"return "hello world";"#,
                Arc::new(EchoDispatcher),
                timeout(),
            )
            .await
            .unwrap();
        assert_eq!(result.value, serde_json::json!("hello world"));
    }

    #[tokio::test]
    async fn eval_object_value() {
        let result = engine()
            .execute(
                r#"return { a: 1, b: "two" };"#,
                Arc::new(EchoDispatcher),
                timeout(),
            )
            .await
            .unwrap();
        assert_eq!(result.value, serde_json::json!({"a": 1, "b": "two"}));
    }

    #[tokio::test]
    async fn console_capture() {
        let result = engine()
            .execute(
                r#"
                console.log("hello");
                console.warn("careful");
                console.error("oops");
                return null;
                "#,
                Arc::new(EchoDispatcher),
                timeout(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.console_output,
            vec!["hello", "[WARN] careful", "[ERROR] oops"]
        );
    }

    #[tokio::test]
    async fn flat_tool_call() {
        let result = engine()
            .execute(
                r#"
                const r = await tools.my_tool({ key: "val" });
                return r;
                "#,
                Arc::new(EchoDispatcher),
                timeout(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.value,
            serde_json::json!({"tool": "my_tool", "args": {"key": "val"}})
        );
        assert_eq!(result.tool_calls_made, 1);
    }

    #[tokio::test]
    async fn nested_namespace_tool_call() {
        let result = engine()
            .execute(
                r#"
                const r = await tools.honeycomb.list_environments({ limit: 10 });
                return r;
                "#,
                Arc::new(EchoDispatcher),
                timeout(),
            )
            .await
            .unwrap();
        // Nested dispatch joins server_toolName
        assert_eq!(
            result.value,
            serde_json::json!({"tool": "honeycomb_list_environments", "args": {"limit": 10}})
        );
        assert_eq!(result.tool_calls_made, 1);
    }

    #[tokio::test]
    async fn tool_call_error_becomes_exception() {
        let result = engine()
            .execute(
                r#"
                try {
                    await tools.bad_tool({});
                    return "should not reach";
                } catch (e) {
                    return { caught: e.message, isToolError: e.isToolError };
                }
                "#,
                Arc::new(FailingDispatcher),
                timeout(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.value,
            serde_json::json!({"caught": "tool call failed", "isToolError": true})
        );
    }

    #[tokio::test]
    async fn timeout_on_infinite_loop() {
        let result = engine()
            .execute(
                "while(true) {}",
                Arc::new(EchoDispatcher),
                Duration::from_millis(200),
            )
            .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            JsEngineError::Timeout(_) | JsEngineError::ScriptRuntime(_) => {}
            other => panic!("expected Timeout or ScriptRuntime, got: {other}"),
        }
    }

    #[tokio::test]
    async fn tool_call_limit_enforced() {
        let mut engine = JsEngine::new();
        engine.max_tool_calls = 3;
        let result = engine
            .execute(
                r#"
                const results = [];
                for (let i = 0; i < 5; i++) {
                    try {
                        await tools.echo({ i });
                        results.push("ok");
                    } catch (e) {
                        results.push("error: " + e.message);
                    }
                }
                return results;
                "#,
                Arc::new(EchoDispatcher),
                timeout(),
            )
            .await
            .unwrap();

        let arr = result.value.as_array().unwrap();
        assert_eq!(arr[0], "ok");
        assert_eq!(arr[1], "ok");
        assert_eq!(arr[2], "ok");
        assert!(arr[3].as_str().unwrap().contains("limit exceeded"));
    }

    #[tokio::test]
    async fn script_syntax_error() {
        let result = engine()
            .execute("return {{{;", Arc::new(EchoDispatcher), timeout())
            .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            JsEngineError::ScriptSyntax(_) => {}
            other => panic!("expected ScriptSyntax, got: {other}"),
        }
    }

    #[tokio::test]
    async fn script_runtime_error() {
        let result = engine()
            .execute(
                "throw new Error('boom');",
                Arc::new(EchoDispatcher),
                timeout(),
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn multiple_tool_calls() {
        let result = engine()
            .execute(
                r#"
                const a = await tools.tool_a({ x: 1 });
                const b = await tools.tool_b({ y: 2 });
                return { a, b };
                "#,
                Arc::new(EchoDispatcher),
                timeout(),
            )
            .await
            .unwrap();
        assert_eq!(result.tool_calls_made, 2);
        assert_eq!(
            result.value,
            serde_json::json!({
                "a": {"tool": "tool_a", "args": {"x": 1}},
                "b": {"tool": "tool_b", "args": {"y": 2}}
            })
        );
    }

    #[tokio::test]
    async fn no_return_gives_null() {
        let result = engine()
            .execute("const x = 42;", Arc::new(EchoDispatcher), timeout())
            .await
            .unwrap();
        // Without an explicit return, the async IIFE returns undefined → null
        assert!(result.value.is_null());
    }

    #[tokio::test]
    async fn parallel_tool_calls_with_promise_all() {
        let result = engine()
            .execute(
                r#"
                const [a, b, c] = await Promise.all([
                    tools.t1({ n: 1 }),
                    tools.t2({ n: 2 }),
                    tools.t3({ n: 3 }),
                ]);
                return [a.tool, b.tool, c.tool];
                "#,
                Arc::new(EchoDispatcher),
                timeout(),
            )
            .await
            .unwrap();
        assert_eq!(result.value, serde_json::json!(["t1", "t2", "t3"]));
        assert_eq!(result.tool_calls_made, 3);
    }
}
