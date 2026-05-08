//! Sandboxed JavaScript runtime wrapper around rquickjs for composing MCP
//! tool calls. Each execution gets a fresh runtime (no state carryover) with
//! configurable resource limits and async tool dispatch via [`ToolDispatcher`].

mod error;

pub use error::JsEngineError;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rquickjs::function::{Opt, Rest};
use rquickjs::prelude::Promised;
use rquickjs::{async_with, AsyncContext, AsyncRuntime, CatchResultExt, Function, Object};

#[async_trait::async_trait]
pub trait ToolDispatcher: Send + Sync + 'static {
    async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String>;
}

#[derive(Debug)]
pub struct ExecutionResult {
    pub value: serde_json::Value,
    pub console_output: Vec<String>,
    pub tool_calls_made: usize,
    pub execution_time: Duration,
}

/// Creates a fresh `AsyncRuntime` + `AsyncContext` per execution with
/// configurable memory, stack, and tool-call limits.
pub struct JsEngine {
    memory_limit: usize,
    stack_limit: usize,
    max_tool_calls: usize,
    /// Cap on a single inner-tool result, in bytes. Scripts can pull large
    /// payloads and filter them before returning.
    max_tool_result_bytes: usize,
    /// Hard ceiling on the script's final return value, in bytes —
    /// defends against runaway scripts that synthesize gigabytes.
    /// Agent-context preservation is *not* this cap's job: callers
    /// (e.g. `Compose`) self-curate the return through the
    /// universal-pipeline walker, which produces an elided envelope
    /// + recovery handle for any return ≥ the walker's threshold.
    /// Setting this anywhere near typical tool-output sizes
    /// pre-empts the walker and defeats the universal pipeline.
    max_return_bytes: usize,
    /// Cap on the console buffer in bytes. Drops oldest entries (tail
    /// truncation) when exceeded. Console is a debug sidecar, not primary
    /// output — kept small so a runaway log loop can never flood the agent
    /// context. Memory is bounded *during* execution, not just at the end.
    max_console_bytes: usize,
}

impl Default for JsEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl JsEngine {
    pub fn new() -> Self {
        Self {
            memory_limit: 8 * 1024 * 1024,
            stack_limit: 512 * 1024,
            max_tool_calls: 50,
            max_tool_result_bytes: 4 * 1024 * 1024,
            max_return_bytes: 16 * 1024 * 1024,
            max_console_bytes: 8 * 1024,
        }
    }

    pub fn with_max_tool_calls(mut self, n: usize) -> Self {
        self.max_tool_calls = n;
        self
    }

    pub fn with_max_tool_result_bytes(mut self, n: usize) -> Self {
        self.max_tool_result_bytes = n;
        self
    }

    pub fn with_max_return_bytes(mut self, n: usize) -> Self {
        self.max_return_bytes = n;
        self
    }

    /// Set the cap on the console buffer (in bytes). When exceeded, oldest
    /// entries are dropped (tail truncation) and a marker line is prepended.
    pub fn with_max_console_bytes(mut self, n: usize) -> Self {
        self.max_console_bytes = n;
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
        let max_tool_result_bytes = self.max_tool_result_bytes;
        let max_return_bytes = self.max_return_bytes;
        let memory_limit = self.memory_limit;
        let stack_limit = self.stack_limit;
        let max_console_bytes = self.max_console_bytes;

        let console_buf: Arc<std::sync::Mutex<ConsoleBuf>> =
            Arc::new(std::sync::Mutex::new(ConsoleBuf::new(max_console_bytes)));
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
                register_console(&ctx, Arc::clone(&console_buf_inner))
                    .map_err(|e| JsEngineError::Runtime(format!("console registration: {e}")))?;

                register_timers(&ctx)
                    .map_err(|e| JsEngineError::Runtime(format!("timer registration: {e}")))?;

                register_tool_bridge(
                    &ctx,
                    Arc::clone(&dispatcher),
                    Arc::clone(&tool_call_count_inner),
                    max_tool_calls,
                    max_tool_result_bytes,
                )
                .map_err(|e| JsEngineError::Runtime(format!("tool bridge registration: {e}")))?;

                let full_script = build_full_script(&script);

                let maybe_promise: rquickjs::promise::MaybePromise<'_> =
                    match ctx.eval(full_script).catch(&ctx) {
                        Ok(v) => v,
                        Err(caught) => {
                            let msg = format_caught_error(&caught);
                            // Interrupt handler raises "interrupted" on timeout.
                            if msg.contains("interrupted") {
                                return Err(JsEngineError::Timeout(timeout));
                            }
                            return Err(JsEngineError::ScriptSyntax(msg));
                        }
                    };

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

            rt.idle().await;

            Ok::<String, JsEngineError>(value_json)
        })
        .await;

        let value_json = match result {
            Ok(inner) => inner?,
            Err(_elapsed) => return Err(JsEngineError::Timeout(timeout)),
        };

        if timed_out.load(Ordering::Relaxed) {
            return Err(JsEngineError::Timeout(timeout));
        }

        let value: serde_json::Value =
            serde_json::from_str(&value_json).unwrap_or(serde_json::Value::Null);

        let result_size = value_json.len();
        if result_size > max_return_bytes {
            return Err(JsEngineError::ReturnTooLarge {
                size: result_size,
                max: max_return_bytes,
            });
        }

        let console_output = console_buf.lock().unwrap().snapshot();
        let tool_calls_made = tool_call_count.load(Ordering::Relaxed);

        Ok(ExecutionResult {
            value,
            console_output,
            tool_calls_made,
            execution_time: start.elapsed(),
        })
    }
}

/// Bounded ring buffer for console output. Drops oldest entries (tail
/// truncation) when bytes exceed `max_bytes`; the last lines carry the
/// most signal (final state, or the op just before a crash).
struct ConsoleBuf {
    lines: VecDeque<String>,
    bytes: usize,
    dropped: usize,
    max_bytes: usize,
}

impl ConsoleBuf {
    fn new(max_bytes: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            bytes: 0,
            dropped: 0,
            max_bytes,
        }
    }

    fn push(&mut self, msg: String) {
        let added = msg.len() + 1; // +1 for implicit newline
        self.bytes += added;
        self.lines.push_back(msg);

        // Always keep at least one line so a single oversized push isn't
        // silently dropped.
        while self.bytes > self.max_bytes && self.lines.len() > 1 {
            if let Some(oldest) = self.lines.pop_front() {
                self.bytes -= oldest.len() + 1;
                self.dropped += 1;
            }
        }
    }

    /// Snapshot without consuming; prepends a `[... N earlier lines dropped]`
    /// marker if any entries were dropped.
    fn snapshot(&self) -> Vec<String> {
        let mut out: Vec<String> = self.lines.iter().cloned().collect();
        if self.dropped > 0 {
            out.insert(0, format!("[... {} earlier lines dropped]", self.dropped));
        }
        out
    }
}

/// Capture `console.log/info/warn/error` output to a shared bounded buffer.
/// Values stringify per JS-runtime semantics (strings unquoted, objects via
/// `JSON.stringify`, circular refs → `[Circular]`); never throws.
fn register_console(
    ctx: &rquickjs::Ctx<'_>,
    buf: Arc<std::sync::Mutex<ConsoleBuf>>,
) -> Result<(), rquickjs::Error> {
    let console = Object::new(ctx.clone())?;

    // Helper function: takes Ctx + Rest with the same 'js lifetime, forcing
    // the calling closure to unify them (closure literals would otherwise
    // assign separate anonymous lifetimes per parameter).
    fn log_with<'js>(
        ctx: rquickjs::Ctx<'js>,
        args: Rest<rquickjs::Value<'js>>,
        prefix: &str,
        buf: &std::sync::Mutex<ConsoleBuf>,
    ) {
        let msg = format_console_args(&ctx, &args.0, prefix);
        buf.lock().unwrap().push(msg);
    }

    let b = Arc::clone(&buf);
    console.set(
        "log",
        Function::new(ctx.clone(), move |ctx, args| log_with(ctx, args, "", &b))?,
    )?;

    let b = Arc::clone(&buf);
    console.set(
        "info",
        Function::new(ctx.clone(), move |ctx, args| log_with(ctx, args, "", &b))?,
    )?;

    let b = Arc::clone(&buf);
    console.set(
        "warn",
        Function::new(ctx.clone(), move |ctx, args| {
            log_with(ctx, args, "[WARN]", &b)
        })?,
    )?;

    let b = Arc::clone(&buf);
    console.set(
        "error",
        Function::new(ctx.clone(), move |ctx, args| {
            log_with(ctx, args, "[ERROR]", &b)
        })?,
    )?;

    ctx.globals().set("console", console)?;
    Ok(())
}

fn register_timers(ctx: &rquickjs::Ctx<'_>) -> Result<(), rquickjs::Error> {
    let next_timer_id = Arc::new(AtomicUsize::new(1));
    let active_timers = Arc::new(std::sync::Mutex::new(HashMap::new()));

    let set_next_timer_id = Arc::clone(&next_timer_id);
    let set_active_timers = Arc::clone(&active_timers);
    ctx.globals().set(
        "setTimeout",
        Function::new(ctx.clone(), move |ctx, callback, delay_ms, args| {
            set_timeout(
                ctx,
                callback,
                delay_ms,
                args,
                Arc::clone(&set_next_timer_id),
                Arc::clone(&set_active_timers),
            )
        })?,
    )?;

    ctx.globals().set(
        "clearTimeout",
        Function::new(ctx.clone(), move |timer_id: usize| {
            if let Some(cancel) = active_timers.lock().unwrap().remove(&timer_id) {
                let _ = cancel.send(());
            }
        })?,
    )?;
    Ok(())
}

fn set_timeout<'js>(
    ctx: rquickjs::Ctx<'js>,
    callback: Function<'js>,
    delay_ms: Opt<f64>,
    args: Rest<rquickjs::Value<'js>>,
    next_timer_id: Arc<AtomicUsize>,
    active_timers: Arc<std::sync::Mutex<HashMap<usize, tokio::sync::oneshot::Sender<()>>>>,
) -> rquickjs::Result<usize> {
    let timer_id = next_timer_id.fetch_add(1, Ordering::Relaxed);
    let (cancel, cancelled) = tokio::sync::oneshot::channel();
    active_timers.lock().unwrap().insert(timer_id, cancel);

    let delay_ms = delay_ms.0.unwrap_or(0.0);
    let millis = if delay_ms.is_finite() && delay_ms > 0.0 {
        delay_ms as u64
    } else {
        0
    };

    ctx.spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(millis)) => {
                if active_timers.lock().unwrap().remove(&timer_id).is_some() {
                    let _ = callback.call::<_, ()>((Rest(args.0),));
                }
            }
            _ = cancelled => {}
        }
    });

    Ok(timer_id)
}

fn format_console_args<'js>(
    ctx: &rquickjs::Ctx<'js>,
    args: &[rquickjs::Value<'js>],
    prefix: &str,
) -> String {
    let parts: Vec<String> = args.iter().map(|v| console_arg_to_string(ctx, v)).collect();
    let joined = parts.join(" ");
    if prefix.is_empty() {
        joined
    } else {
        format!("{prefix} {joined}")
    }
}

/// Coerce a JS value to its console-friendly string, matching JS-runtime
/// semantics (strings unquoted, objects via JSON, circular → `[Circular]`).
fn console_arg_to_string<'js>(ctx: &rquickjs::Ctx<'js>, v: &rquickjs::Value<'js>) -> String {
    if v.is_undefined() {
        return "undefined".into();
    }
    if v.is_null() {
        return "null".into();
    }
    if v.is_bool() {
        return v
            .as_bool()
            .map(|b| b.to_string())
            .unwrap_or_else(|| "false".into());
    }
    if v.is_string() {
        return v
            .as_string()
            .and_then(|s| s.to_string().ok())
            .unwrap_or_default();
    }
    if v.is_number() {
        // Special-case ±Infinity to match JS output (Rust's f64::Display
        // writes "inf"/"-inf").
        let n = v.as_number().unwrap_or(0.0);
        if n.is_infinite() {
            return if n.is_sign_negative() {
                "-Infinity".into()
            } else {
                "Infinity".into()
            };
        }
        return n.to_string();
    }
    if v.is_function() {
        return "[Function]".into();
    }
    if v.is_symbol() {
        return "[Symbol]".into();
    }
    // Circular refs make JSON.stringify throw TypeError; fall back so
    // console calls never abort the script.
    json_stringify(ctx, v).unwrap_or_else(|_| "[Circular]".into())
}

fn json_stringify<'js>(
    ctx: &rquickjs::Ctx<'js>,
    v: &rquickjs::Value<'js>,
) -> rquickjs::Result<String> {
    let json: Object = ctx.globals().get("JSON")?;
    let stringify: Function = json.get("stringify")?;
    stringify.call((v.clone(),))
}

/// `__call_tool_raw(name, args_json)` → Promise<string>. Returns the
/// JSON-encoded envelope `{"ok":true,"value":...}` or
/// `{"ok":false,"error":"..."}`; the JS bootstrap unwraps it.
fn register_tool_bridge(
    ctx: &rquickjs::Ctx<'_>,
    dispatcher: Arc<dyn ToolDispatcher>,
    tool_call_count: Arc<AtomicUsize>,
    max_tool_calls: usize,
    max_tool_result_bytes: usize,
) -> Result<(), rquickjs::Error> {
    ctx.globals().set(
        "__call_tool_raw",
        Function::new(ctx.clone(), move |name: String, args_json: String| {
            let d = Arc::clone(&dispatcher);
            let tc = Arc::clone(&tool_call_count);
            Promised(async move {
                let count = tc.fetch_add(1, Ordering::Relaxed);
                if count >= max_tool_calls {
                    return encode_error(&format!(
                        "Tool call limit exceeded ({max_tool_calls} max)"
                    ));
                }

                let args: serde_json::Value = match serde_json::from_str(&args_json) {
                    Ok(v) => v,
                    Err(e) => return encode_error(&format!("Invalid arguments JSON: {e}")),
                };

                match d.call_tool(&name, args).await {
                    Ok(result) => {
                        let result_json =
                            serde_json::to_string(&result).unwrap_or_else(|_| "null".into());
                        if result_json.len() > max_tool_result_bytes {
                            return encode_error(&format!(
                                "Tool result too large ({} bytes, max {max_tool_result_bytes}). \
                                 The tool returned more data than compose can hold in a single \
                                 result. Try a more specific upstream query (limit/filter \
                                 parameters), or recover the persisted bytes via \
                                 tool_output_fetch(invocation_id, query).",
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

/// Builds the `{"ok":false,"error":...}` envelope. Delegates string
/// escaping to `serde_json` so control characters (newlines, tabs)
/// inside the message are encoded as `\n`, `\t`, etc. — otherwise
/// `JSON.parse(raw)` in `__dispatch` chokes with a generic
/// "Bad control character in string literal" and the agent never
/// sees the underlying tool error.
fn encode_error(msg: &str) -> String {
    let escaped = serde_json::to_string(msg)
        .unwrap_or_else(|_| "\"<error message could not be encoded>\"".into());
    format!(r#"{{"ok":false,"error":{escaped}}}"#)
}

/// Bootstrap (tools proxy) + user code wrapped in an async IIFE for
/// top-level `await` and `return`. The tools proxy supports both flat
/// (`tools.prefixed_name(args)`) and nested (`tools.server.toolName(args)`)
/// calling conventions via a two-level `Proxy` chain.
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
    async fn console_log_accepts_objects() {
        let result = engine()
            .execute(
                r#"console.log("build:", { id: 5905454, status: "failed" }); return null;"#,
                Arc::new(EchoDispatcher),
                timeout(),
            )
            .await
            .unwrap();
        assert_eq!(result.console_output.len(), 1);
        assert!(result.console_output[0].starts_with("build: "));
        assert!(result.console_output[0].contains("\"id\":5905454"));
        assert!(result.console_output[0].contains("\"status\":\"failed\""));
    }

    #[tokio::test]
    async fn console_log_accepts_mixed_types() {
        let result = engine()
            .execute(
                r#"console.log("count:", 42, true, null, undefined); return null;"#,
                Arc::new(EchoDispatcher),
                timeout(),
            )
            .await
            .unwrap();
        assert_eq!(result.console_output, vec!["count: 42 true null undefined"]);
    }

    #[tokio::test]
    async fn console_log_accepts_arrays() {
        let result = engine()
            .execute(
                r#"console.log("ids:", [1, 2, 3]); return null;"#,
                Arc::new(EchoDispatcher),
                timeout(),
            )
            .await
            .unwrap();
        assert_eq!(result.console_output, vec!["ids: [1,2,3]"]);
    }

    #[tokio::test]
    async fn console_log_handles_circular_reference() {
        let result = engine()
            .execute(
                r#"const a = {}; a.self = a; console.log(a); return "ok";"#,
                Arc::new(EchoDispatcher),
                timeout(),
            )
            .await
            .unwrap();
        assert_eq!(result.value, serde_json::json!("ok"));
        // Should not have aborted — circular reference handled gracefully
        assert!(!result.console_output.is_empty());
        assert_eq!(result.console_output[0], "[Circular]");
    }

    #[tokio::test]
    async fn console_warn_keeps_prefix_with_object_arg() {
        let result = engine()
            .execute(
                r#"console.warn("careful:", { issue: "x" }); return null;"#,
                Arc::new(EchoDispatcher),
                timeout(),
            )
            .await
            .unwrap();
        assert!(result.console_output[0].starts_with("[WARN] careful: "));
        assert!(result.console_output[0].contains("\"issue\":\"x\""));
    }

    #[tokio::test]
    async fn console_buffer_truncates_to_tail() {
        let engine = JsEngine::new().with_max_console_bytes(200);
        let result = engine
            .execute(
                r#"
                for (let i = 0; i < 100; i++) {
                    console.log("line " + i);
                }
                return "done";
                "#,
                Arc::new(EchoDispatcher),
                timeout(),
            )
            .await
            .unwrap();

        // First entry is the dropped-count marker
        assert!(result.console_output[0].starts_with("[..."));
        assert!(result.console_output[0].contains("dropped"));

        // Last entry should be near the end of the loop, not the start —
        // tail truncation keeps the most recent lines.
        let last = result.console_output.last().unwrap();
        assert!(last.starts_with("line 9"), "tail not preserved: {last}");
    }

    #[tokio::test]
    async fn console_buffer_under_cap_no_marker() {
        let result = engine()
            .execute(
                r#"
                console.log("a");
                console.log("b");
                console.log("c");
                return "done";
                "#,
                Arc::new(EchoDispatcher),
                timeout(),
            )
            .await
            .unwrap();

        // No marker — all three short lines fit well under the 8 KB default.
        assert_eq!(result.console_output, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn console_buffer_marker_reflects_drop_count() {
        let engine = JsEngine::new().with_max_console_bytes(50);
        let result = engine
            .execute(
                r#"
                for (let i = 0; i < 20; i++) console.log("xxxxxxxxxx");
                return "done";
                "#,
                Arc::new(EchoDispatcher),
                timeout(),
            )
            .await
            .unwrap();

        // The first entry is the marker; the dropped count parsed out of it
        // should reflect that most of the 20 lines were truncated.
        let marker = &result.console_output[0];
        assert!(marker.contains("dropped"), "no marker: {marker}");
        let parsed: Option<usize> = marker.split_whitespace().find_map(|w| w.parse().ok());
        assert!(
            parsed.unwrap_or(0) > 10,
            "expected >10 drops, got: {marker}"
        );
    }

    #[tokio::test]
    async fn console_buffer_keeps_at_least_one_line() {
        // A single very large line should still appear. The ring buffer
        // never drops to empty — preserving a single oversized line is
        // strictly better than silently swallowing it.
        let engine = JsEngine::new().with_max_console_bytes(10);
        let result = engine
            .execute(
                r#"console.log("this is much longer than 10 bytes"); return "done";"#,
                Arc::new(EchoDispatcher),
                timeout(),
            )
            .await
            .unwrap();

        assert!(result
            .console_output
            .iter()
            .any(|s| s.contains("longer than")));
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
    async fn set_timeout_resolves_promise_sleep() {
        let result = engine()
            .execute(
                r#"
                const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
                let done = false;
                await sleep(10);
                done = true;
                return done;
                "#,
                Arc::new(EchoDispatcher),
                timeout(),
            )
            .await
            .unwrap();

        assert_eq!(result.value, serde_json::json!(true));
    }

    #[tokio::test]
    async fn set_timeout_returns_handle_and_supports_callback_args() {
        let result = engine()
            .execute(
                r#"
                return await new Promise(resolve => {
                    const id = setTimeout(resolve, 10, "ready");
                    if (typeof id !== "number") resolve("bad id");
                });
                "#,
                Arc::new(EchoDispatcher),
                timeout(),
            )
            .await
            .unwrap();

        assert_eq!(result.value, serde_json::json!("ready"));
    }

    #[tokio::test]
    async fn fire_and_forget_timeout_runs_before_completion() {
        let result = engine()
            .execute(
                r#"
                setTimeout(() => console.log("timer fired"), 10);
                return "done";
                "#,
                Arc::new(EchoDispatcher),
                timeout(),
            )
            .await
            .unwrap();

        assert_eq!(result.value, serde_json::json!("done"));
        assert_eq!(result.console_output, vec!["timer fired"]);
    }

    #[tokio::test]
    async fn clear_timeout_aborts_pending_sleep() {
        let result = engine()
            .execute(
                r#"
                const id = setTimeout(() => console.log("should not run"), 500);
                clearTimeout(id);
                return "done";
                "#,
                Arc::new(EchoDispatcher),
                Duration::from_millis(100),
            )
            .await
            .unwrap();

        assert_eq!(result.value, serde_json::json!("done"));
        assert!(result.console_output.is_empty());
    }

    #[tokio::test]
    async fn set_timeout_delay_counts_against_execution_timeout() {
        let result = engine()
            .execute(
                r#"
                await new Promise(resolve => setTimeout(resolve, 500));
                return "late";
                "#,
                Arc::new(EchoDispatcher),
                Duration::from_millis(50),
            )
            .await;

        assert!(matches!(result, Err(JsEngineError::Timeout(_))));
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

    /// Per-tool-result cap is independent of the final-return cap: a script
    /// can pull a large inner result, summarize it, and return a small value.
    #[tokio::test]
    async fn separate_caps_let_script_filter_large_tool_results() {
        // Dispatcher returns ~500 KB of data
        struct LargeDispatcher;
        #[async_trait::async_trait]
        impl ToolDispatcher for LargeDispatcher {
            async fn call_tool(
                &self,
                _name: &str,
                _args: serde_json::Value,
            ) -> Result<serde_json::Value, String> {
                let big = "x".repeat(500 * 1024);
                Ok(serde_json::json!({ "data": big }))
            }
        }

        let engine = JsEngine::new()
            .with_max_tool_result_bytes(1024 * 1024)
            .with_max_return_bytes(1024);

        let result = engine
            .execute(
                r#"
                const r = await tools.big_tool({});
                // Summarize the large payload to a tiny return value
                return { len: r.data.length };
                "#,
                Arc::new(LargeDispatcher),
                timeout(),
            )
            .await
            .unwrap();

        assert_eq!(result.value, serde_json::json!({ "len": 500 * 1024 }));
        assert_eq!(result.tool_calls_made, 1);
    }

    /// The return cap blocks oversized script output even when no tool
    /// calls happen. Distinct error variant from the per-tool cap.
    #[tokio::test]
    async fn return_cap_blocks_oversized_script_output() {
        let engine = JsEngine::new().with_max_return_bytes(1024);

        let result = engine
            .execute(
                r#"return "x".repeat(5 * 1024);"#,
                Arc::new(EchoDispatcher),
                timeout(),
            )
            .await;

        match result.unwrap_err() {
            JsEngineError::ReturnTooLarge { size, max } => {
                assert!(size > max);
                assert_eq!(max, 1024);
            }
            other => panic!("expected ReturnTooLarge, got: {other}"),
        }
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
