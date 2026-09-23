use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rquickjs::{prelude::Promised, CatchResultExt, Ctx, Exception, Function, Module, Value};
use serde::Serialize;
use sha2::{Digest, Sha256};

pub const LOAD_SCRIPT_DECLARATION: &str =
    "declare function loadScript<T = unknown>(path: string): Promise<T>;";

#[derive(Clone, Debug, Serialize)]
pub struct SourceIdentity {
    pub path: String,
    pub sha256: String,
    pub byte_length: usize,
}

#[async_trait::async_trait]
pub trait ScriptSourceProvider: Send + Sync + 'static {
    /// Read complete bytes through the caller's ordinary authorized file-read path.
    /// Called only on cache misses. Source validation and budgets belong to the loader.
    async fn read(&self, path: &str) -> Result<Vec<u8>, String>;
}

// Require one canonical cache key, using the space URI grammar. The authorized
// facade still applies its ordinary path rules and permissions on each read.
fn validate_path(path: &str) -> Result<(), String> {
    let (slug, relative) = path
        .strip_prefix("space:")
        .and_then(|rest| rest.split_once('/'))
        .ok_or("invalid_path: expected space:<slug>/<path>.js")?;
    if slug.is_empty()
        || slug.starts_with('-')
        || slug.ends_with('-')
        || slug.contains("--")
        || !slug
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        || !relative.ends_with(".js")
        || relative.chars().any(char::is_control)
        || relative.contains('\\')
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err("invalid_path: expected canonical space:<slug>/<path>.js".into());
    }
    Ok(())
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
    let check_audit = audit.clone();
    let check_limits = limits.clone();
    ctx.globals().set(
        "__script_check",
        Function::new(ctx.clone(), move |ctx, parent, path| {
            check(ctx, parent, path, check_audit.clone(), check_limits.clone())
        })?,
    )?;

    let compile_audit = audit.clone();
    ctx.globals().set(
        "__script_compile",
        Function::new(ctx.clone(), move |ctx, path| {
            compile(
                ctx,
                path,
                provider.clone(),
                limits.clone(),
                compile_audit.clone(),
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
    audit: SharedScriptAudit,
    limits: ScriptLimits,
) -> rquickjs::Result<()> {
    let result = (|| {
        reserve_load(&audit, &limits)?;
        validate_path(&path)?;
        reserve_dependency(&audit, &limits, &parent, &path)
    })();
    result.map_err(|message| {
        record_error(&audit, message.clone());
        Exception::throw_message(&ctx, &message)
    })
}

fn compile<'js>(
    ctx: Ctx<'js>,
    path: String,
    provider: Option<Arc<dyn ScriptSourceProvider>>,
    limits: ScriptLimits,
    audit: SharedScriptAudit,
) -> Promised<impl Future<Output = rquickjs::Result<Function<'js>>> + 'js> {
    Promised(async move {
        let start = Instant::now();
        {
            let state = audit.lock().unwrap();
            if let Some(error) = &state.termination {
                return Err(Exception::throw_message(&ctx, error));
            }
        }
        let bytes = provider
            .ok_or_else(|| {
                Exception::throw_message(&ctx, "access_denied: script provider unavailable")
            })?
            .read(&path)
            .await
            .map_err(|message| {
                record_error(&audit, format!("{path}: {message}"));
                Exception::throw_message(&ctx, &format!("{path}: {message}"))
            })?;
        {
            let mut state = audit.lock().unwrap();
            if let Some(error) = &state.termination {
                return Err(Exception::throw_message(&ctx, error));
            }
            // Storage has already allocated the bytes. Reserve the source budget
            // atomically before UTF-8 conversion or QuickJS compilation.
            state.bytes = state.bytes.saturating_add(bytes.len());
            if bytes.len() > limits.max_file_bytes || state.bytes > limits.max_total_bytes {
                let message = format!("size_limit: source budget exceeded at {path}");
                state.termination = Some(message.clone());
                state.errors.push(message.clone());
                return Err(Exception::throw_message(&ctx, &message));
            }
            state.sources.push(SourceAudit {
                identity: SourceIdentity {
                    path: path.clone(),
                    sha256: format!("{:x}", Sha256::digest(&bytes)),
                    byte_length: bytes.len(),
                },
                duration_ms: start.elapsed().as_millis() as u64,
                error: None,
                initialized: false,
            });
        }
        let source = String::from_utf8(bytes).map_err(|_| {
            let message = format!("invalid_source: {path} is not UTF-8");
            record_error(&audit, message.clone());
            Exception::throw_message(&ctx, &message)
        })?;
        // Prefix on the first line preserves original file line numbers.
        let wrapped = format!(
            "export default async function(tools, console, setTimeout, loadScript) {{{}\n}}",
            source
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
        check(parent, path);
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
