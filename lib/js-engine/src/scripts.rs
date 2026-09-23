use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rquickjs::{prelude::Promised, CatchResultExt, Ctx, Exception, Function, Module, Value};
use serde::Serialize;

pub const LOAD_SCRIPT_DECLARATION: &str =
    "declare function loadScript<T = unknown>(path: string): Promise<T>;";
pub const LOAD_SCRIPT_DOC: &str = "Load authorized space:<slug>/<path>.js files with await loadScript(path). Files are async function bodies: return exports explicitly, then call their functions with arguments. Dependencies use the same loader and caller permissions. Initialization is cached per invocation; keep it pure and put mutations in exported functions.";

#[derive(Clone, Debug, Serialize)]
pub struct SourceIdentity {
    pub path: String,
    pub revision: String,
    pub blob_oid: String,
    pub sha256: String,
    pub byte_length: usize,
}

pub struct ScriptSource {
    pub identity: SourceIdentity,
    pub text: String,
}

#[async_trait::async_trait]
pub trait ScriptSourceProvider: Send + Sync + 'static {
    /// Validate the canonical address and check current access, including cache hits.
    async fn authorize(&self, path: &str) -> Result<(), String>;
    /// Read at one execution-local revision, enforcing max_bytes before copying the blob.
    async fn read(&self, path: &str, max_bytes: usize) -> Result<ScriptSource, String>;
}

#[derive(Clone, Debug)]
pub struct ScriptLimits {
    pub max_file_bytes: usize,
    pub max_total_bytes: usize,
    pub max_loads: usize,
    pub max_dependency_depth: usize,
}

impl Default for ScriptLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 256 * 1024,
            max_total_bytes: 1024 * 1024,
            max_loads: 64,
            max_dependency_depth: 16,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct SourceAudit {
    #[serde(flatten)]
    pub identity: SourceIdentity,
    pub duration_ms: u64,
    pub error: Option<String>,
    pub initialized: bool,
}

#[derive(Default, Serialize)]
pub struct ScriptAudit {
    pub sources: Vec<SourceAudit>,
    pub dependencies: BTreeMap<String, BTreeSet<String>>,
    pub errors: Vec<String>,
    #[serde(skip)]
    loads: usize,
    #[serde(skip)]
    bytes: usize,
    #[serde(skip)]
    pub(crate) termination: Option<String>,
}

pub type SharedScriptAudit = Arc<Mutex<ScriptAudit>>;

fn chain(graph: &BTreeMap<String, BTreeSet<String>>, from: &str, to: &str) -> Option<Vec<String>> {
    chain_inner(graph, from, to, &mut BTreeSet::new())
}

fn chain_inner(
    graph: &BTreeMap<String, BTreeSet<String>>,
    from: &str,
    to: &str,
    visited: &mut BTreeSet<String>,
) -> Option<Vec<String>> {
    if from == to {
        return Some(vec![from.to_owned()]);
    }
    if !visited.insert(from.to_owned()) {
        return None;
    }
    for next in graph.get(from).into_iter().flatten() {
        if let Some(mut path) = chain_inner(graph, next, to, visited) {
            path.insert(0, from.to_owned());
            return Some(path);
        }
    }
    None
}

fn depth(graph: &BTreeMap<String, BTreeSet<String>>, root: &str) -> usize {
    fn visit(
        graph: &BTreeMap<String, BTreeSet<String>>,
        root: &str,
        memo: &mut BTreeMap<String, usize>,
    ) -> usize {
        if let Some(depth) = memo.get(root) {
            return *depth;
        }
        let depth = graph
            .get(root)
            .into_iter()
            .flatten()
            .map(|next| 1 + visit(graph, next, memo))
            .max()
            .unwrap_or(0);
        memo.insert(root.to_owned(), depth);
        depth
    }
    visit(graph, root, &mut BTreeMap::new())
}

fn reserve_load(audit: &SharedScriptAudit, limits: &ScriptLimits) -> Result<(), String> {
    let mut state = audit.lock().unwrap();
    if let Some(error) = &state.termination {
        return Err(error.clone());
    }
    state.loads += 1;
    if state.loads > limits.max_loads {
        let error = format!("load_limit: maximum {} requests", limits.max_loads);
        state.termination = Some(error.clone());
        return Err(error);
    }
    Ok(())
}

fn reserve_dependency(
    audit: &SharedScriptAudit,
    limits: &ScriptLimits,
    parent: &str,
    path: &str,
) -> Result<(), String> {
    let mut state = audit.lock().unwrap();
    if let Some(error) = &state.termination {
        return Err(error.clone());
    }
    if let Some(mut cycle) = chain(&state.dependencies, path, parent) {
        cycle.insert(0, parent.to_owned());
        return Err(format!("dependency_cycle: {}", cycle.join(" -> ")));
    }
    state
        .dependencies
        .entry(parent.to_owned())
        .or_default()
        .insert(path.to_owned());
    if depth(&state.dependencies, "<compose>") > limits.max_dependency_depth {
        let error = format!(
            "depth_limit: maximum {} dependencies at {path}",
            limits.max_dependency_depth
        );
        state.termination = Some(error.clone());
        return Err(error);
    }
    Ok(())
}

fn record_error(audit: &SharedScriptAudit, message: String) {
    let mut state = audit.lock().unwrap();
    if message.contains("size_limit:") || message.contains("out of memory") {
        state.termination = Some(message.clone());
    }
    state.errors.push(message);
}

pub(crate) fn register(
    ctx: &Ctx<'_>,
    provider: Option<Arc<dyn ScriptSourceProvider>>,
    limits: ScriptLimits,
    audit: SharedScriptAudit,
) -> rquickjs::Result<()> {
    let provider_check = provider.clone();
    let check_audit = audit.clone();
    let check_limits = limits.clone();
    ctx.globals().set(
        "__script_check",
        Function::new(ctx.clone(), move |ctx, parent, path| {
            check(
                ctx,
                parent,
                path,
                provider_check.clone(),
                check_audit.clone(),
                check_limits.clone(),
            )
        })?,
    )?;

    let compile_audit = audit.clone();
    let read_gate = Arc::new(tokio::sync::Mutex::new(()));
    ctx.globals().set(
        "__script_compile",
        Function::new(ctx.clone(), move |ctx, path| {
            compile(
                ctx,
                path,
                provider.clone(),
                limits.clone(),
                compile_audit.clone(),
                read_gate.clone(),
            )
        })?,
    )?;
    ctx.globals().set(
        "__script_finish",
        Function::new(
            ctx.clone(),
            move |path: String, duration_ms: u64, error: Option<String>| {
                let mut state = audit.lock().unwrap();
                if let Some(source) = state.sources.iter_mut().find(|s| s.identity.path == path) {
                    source.duration_ms = duration_ms;
                    source.initialized = error.is_none();
                    source.error = error.clone();
                }
                if let Some(error) = error {
                    state.errors.push(error);
                }
            },
        )?,
    )?;
    Ok(())
}

fn check<'js>(
    ctx: Ctx<'js>,
    parent: String,
    path: String,
    provider: Option<Arc<dyn ScriptSourceProvider>>,
    audit: SharedScriptAudit,
    limits: ScriptLimits,
) -> Promised<impl Future<Output = rquickjs::Result<()>> + 'js> {
    Promised(async move {
        let result = async {
            reserve_load(&audit, &limits)?;
            provider
                .ok_or("access_denied: script provider unavailable")?
                .authorize(&path)
                .await?;
            reserve_dependency(&audit, &limits, &parent, &path)
        }
        .await;
        result.map_err(|message| {
            record_error(&audit, message.clone());
            Exception::throw_message(&ctx, &message)
        })
    })
}

fn compile<'js>(
    ctx: Ctx<'js>,
    path: String,
    provider: Option<Arc<dyn ScriptSourceProvider>>,
    limits: ScriptLimits,
    audit: SharedScriptAudit,
    read_gate: Arc<tokio::sync::Mutex<()>>,
) -> Promised<impl Future<Output = rquickjs::Result<Function<'js>>> + 'js> {
    Promised(async move {
        let start = Instant::now();
        let read_guard = read_gate.lock().await;
        let max_bytes = {
            let state = audit.lock().unwrap();
            if let Some(error) = &state.termination {
                return Err(Exception::throw_message(&ctx, error));
            }
            limits
                .max_file_bytes
                .min(limits.max_total_bytes.saturating_sub(state.bytes))
        };
        let source = provider
            .ok_or_else(|| {
                Exception::throw_message(&ctx, "access_denied: script provider unavailable")
            })?
            .read(&path, max_bytes)
            .await
            .map_err(|message| {
                record_error(&audit, format!("{path}: {message}"));
                Exception::throw_message(&ctx, &format!("{path}: {message}"))
            })?;
        {
            let mut state = audit.lock().unwrap();
            state.bytes = state.bytes.saturating_add(source.text.len());
            state.sources.push(SourceAudit {
                identity: source.identity,
                duration_ms: start.elapsed().as_millis() as u64,
                error: None,
                initialized: false,
            });
            if source.text.len() > limits.max_file_bytes || state.bytes > limits.max_total_bytes {
                let message = format!("size_limit: source budget exceeded at {path}");
                state.termination = Some(message.clone());
                return Err(Exception::throw_message(&ctx, &message));
            }
        }
        drop(read_guard);
        // Prefix on the first line preserves original file line numbers.
        let wrapped = format!(
            "export default async function(tools, console, setTimeout, loadScript) {{{}\n}}",
            source.text
        );
        let result = async {
            let module = Module::declare(ctx.clone(), path.clone(), wrapped)?;
            let (module, evaluated) = module.eval()?;
            evaluated.into_future::<Value>().await?;
            module.get::<_, Function>("default")
        }
        .await
        .catch(&ctx);
        match result {
            Ok(function) => Ok(function),
            Err(error) => {
                record_error(
                    &audit,
                    format!("{path}: {}", super::format_caught_error(&error)),
                );
                Err(error.throw(&ctx))
            }
        }
    })
}

pub(crate) const BOOTSTRAP: &str = r#"
const loadScript = ((check, compile, finish) => {
    const cache = new Map();
    const bound = parent => async path => {
        if (typeof path !== 'string') throw new TypeError('invalid_path: loadScript expects a string');
        await check(parent, path);
        if (!cache.has(path)) {
            const started = Date.now();
            const initialization = (async () => {
                try {
                    const body = await compile(path);
                    const value = await body(tools, console, setTimeout, bound(path));
                    finish(path, Date.now() - started, null);
                    return value;
                } catch (error) {
                    finish(path, Date.now() - started, String(error) + '\n' + String(error && error.stack || ''));
                    throw error;
                }
            })();
            cache.set(path, initialization);
        }
        return await cache.get(path);
    };
    return bound('<compose>');
})(__script_check, __script_compile, __script_finish);
delete globalThis.__script_check;
delete globalThis.__script_compile;
delete globalThis.__script_finish;
"#;
