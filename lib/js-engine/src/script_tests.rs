use super::*;
use std::sync::atomic::AtomicUsize;
use std::sync::Mutex;

struct Provider {
    files: Mutex<HashMap<String, Vec<u8>>>,
    reads: AtomicUsize,
    revoke_after: usize,
    delay: Duration,
}

impl Provider {
    fn new(files: &[(&str, &str)]) -> Arc<Self> {
        Arc::new(Self {
            files: Mutex::new(
                files
                    .iter()
                    .map(|(p, s)| (format!("space:test/{p}.js"), s.as_bytes().to_vec()))
                    .collect(),
            ),
            reads: AtomicUsize::new(0),
            revoke_after: usize::MAX,
            delay: Duration::ZERO,
        })
    }
}

#[async_trait::async_trait]
impl ScriptSourceProvider for Provider {
    async fn read(&self, path: &str) -> Result<Vec<u8>, String> {
        if self.reads.fetch_add(1, Ordering::SeqCst) >= self.revoke_after {
            return Err("access_denied: revoked".into());
        }
        if !path.starts_with("space:test/") {
            return Err("access_denied".into());
        }
        tokio::time::sleep(self.delay).await;
        self.files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or("missing_file".into())
    }
}

struct Echo;
#[tokio::test]
async fn zero_preserves_unlimited_native_memory_and_stack_configuration() {
    let result = run(
        &JsEngine::new().with_memory_limit(0).with_stack_limit(0),
        "return 42;",
        Provider::new(&[]),
    )
    .await
    .unwrap();
    assert_eq!(result.value, 42);
}

#[async_trait::async_trait]
impl ToolDispatcher for Echo {
    async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({"name": name, "args": args, "caller": "original"}))
    }
}

async fn run(
    engine: &JsEngine,
    source: &str,
    provider: Arc<Provider>,
) -> Result<ExecutionResult, JsEngineError> {
    engine
        .execute_with_sources(
            source,
            Arc::new(Echo),
            Duration::from_secs(2),
            Some(provider),
            Arc::default(),
        )
        .await
}

#[tokio::test]
async fn live_exports_dependencies_arguments_and_shared_initialization() {
    let provider = Provider::new(&[
        ("shared", "console.log('initialized'); const local = 7; return { local, echo: async input => await tools.echo(input) };"),
        ("a", "const local = 1; const shared = await loadScript('space:test/shared.js'); return { local, shared };"),
        ("b", "const local = 2; const shared = await loadScript('space:test/shared.js'); return { local, shared };"),
    ]);
    let result = run(&JsEngine::new(), r#"
        const [a,b] = await Promise.all([loadScript('space:test/a.js'),loadScript('space:test/b.js')]);
        const again = await loadScript('space:test/shared.js');
        return { same: a.shared === b.shared && again === a.shared, locals: [a.local,b.local,again.local],
            echoed: await again.echo({ text: "'\"雪; throw new Error('injected')" }) };
    "#, provider.clone()).await.unwrap();
    assert_eq!(provider.reads.load(Ordering::SeqCst), 3);
    assert_eq!(result.console_output, ["initialized"]);
    assert_eq!(result.value["same"], true);
    assert_eq!(result.value["locals"], serde_json::json!([1, 2, 7]));
    assert_eq!(result.value["echoed"]["caller"], "original");
    assert_eq!(
        result.value["echoed"]["args"]["text"],
        "'\"雪; throw new Error('injected')"
    );
    assert_eq!(result.tool_calls_made, 1);
}

#[tokio::test]
async fn invocation_isolation_and_undefined_exports() {
    let provider = Provider::new(&[
        (
            "a",
            "globalThis.count = (globalThis.count || 0) + 1; return {count: globalThis.count};",
        ),
        ("empty", "const local = 1;"),
    ]);
    for _ in 0..2 {
        let result = run(&JsEngine::new(), "const a = await loadScript('space:test/a.js'); return [a.count, await loadScript('space:test/empty.js') === undefined];", provider.clone()).await.unwrap();
        assert_eq!(result.value, serde_json::json!([1, true]));
    }
    assert_eq!(provider.reads.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn fresh_invocations_observe_changed_sources_and_retry_failures() {
    let engine = JsEngine::new();
    let provider = Provider::new(&[("a", "throw new Error('old source');")]);
    let script = "return await loadScript('space:test/a.js');";
    assert!(run(&engine, script, provider.clone())
        .await
        .unwrap_err()
        .to_string()
        .contains("old source"));
    provider
        .files
        .lock()
        .unwrap()
        .insert("space:test/a.js".into(), b"return {version: 2};".to_vec());
    assert_eq!(
        run(&engine, script, provider.clone()).await.unwrap().value["version"],
        2
    );
    assert_eq!(provider.reads.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn concurrent_cache_hits_share_one_read_and_live_object() {
    let mut provider = Provider::new(&[("a", "console.log('once'); return {};")]);
    Arc::get_mut(&mut provider).unwrap().delay = Duration::from_millis(10);
    let result = run(&JsEngine::new(), "const [a,b] = await Promise.all([loadScript('space:test/a.js'),loadScript('space:test/a.js')]); a.live = 7; return [a === b, b.live];", provider.clone()).await.unwrap();
    assert_eq!(result.value, serde_json::json!([true, 7]));
    assert_eq!(result.console_output, ["once"]);
    assert_eq!(provider.reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn invalid_paths_never_reach_the_provider() {
    let provider = Provider::new(&[]);
    for path in [
        "file:/a.js",
        "https://example/a.js",
        "space:/a.js",
        "space:Bad/a.js",
        "space:-test/a.js",
        "space:test-/a.js",
        "space:te--st/a.js",
        "space:test/../a.js",
        "space:test/./a.js",
        "space:test//a.js",
        "space:test/a\\b.js",
        "space:test/a\nb.js",
        "space:test/",
        "space:test/a.ts",
    ] {
        let script = format!(
            "return await loadScript({});",
            serde_json::to_string(path).unwrap()
        );
        assert!(
            run(&JsEngine::new(), &script, provider.clone())
                .await
                .unwrap_err()
                .to_string()
                .contains("invalid_path"),
            "{path}"
        );
    }
    assert_eq!(provider.reads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn loader_validates_utf8_and_audits_exact_source_bytes() {
    let provider = Provider::new(&[("sub/雪 %20 #?", "return '雪';")]);
    provider
        .files
        .lock()
        .unwrap()
        .insert("space:test/bad.js".into(), vec![0xff]);
    let audit: SharedScriptAudit = Arc::default();
    let result = JsEngine::new().execute_with_sources(
        "try {await loadScript('space:test/bad.js')} catch(e) {console.log(String(e))} return await loadScript('space:test/sub/雪 %20 #?.js');",
        Arc::new(Echo), Duration::from_secs(2), Some(provider), audit.clone()
    ).await.unwrap();
    assert_eq!(result.value, "雪");
    assert!(result.console_output[0].contains("invalid_source"));
    let audit = audit.lock().unwrap();
    assert_eq!(audit.sources[0].identity.byte_length, 1);
    assert!(!audit.sources[0].initialized);
    assert!(audit.sources[0].error.as_ref().unwrap().contains("UTF-8"));
    let source = &audit.sources[1];
    assert_eq!(source.identity.byte_length, 13);
    assert_eq!(
        source.identity.sha256,
        "facffa33d8bdfec35dd3ed9ef9cf4cfa8af6f24fd53403e59d4c98a9935065e8"
    );
    assert!(source.initialized);
    let serialized = serde_json::to_value(&*audit).unwrap();
    assert!(serialized["sources"][1].get("revision").is_none());
    assert!(serialized["sources"][1].get("blob_oid").is_none());
}

#[tokio::test]
async fn source_limits_count_bytes_and_unique_files() {
    let source = "return '雪';";
    let provider = Provider::new(&[("a", source), ("b", source)]);
    let limits = ScriptLimits {
        max_file_bytes: source.len(),
        max_total_bytes: source.len(),
        ..Default::default()
    };
    let result = run(
        &JsEngine::new().with_script_limits(limits.clone()),
        "await loadScript('space:test/a.js'); return await loadScript('space:test/a.js');",
        provider.clone(),
    )
    .await
    .unwrap();
    assert_eq!(result.value, "雪");
    for (limits, script) in [
        (ScriptLimits {max_file_bytes: source.len()-1, ..limits.clone()}, "return await loadScript('space:test/a.js');"),
        (limits, "return await Promise.all([loadScript('space:test/a.js'),loadScript('space:test/b.js')]);"),
    ] {
        assert!(run(&JsEngine::new().with_script_limits(limits), script, provider.clone()).await.unwrap_err().to_string().contains("size_limit"));
    }
}

#[tokio::test]
async fn cycles_include_concurrent_branches_and_late_exported_loaders() {
    for (files, inline) in [
        (vec![("a", "return await loadScript('space:test/a.js');")], "return await loadScript('space:test/a.js');"),
        (vec![("a", "return await loadScript('space:test/b.js');"), ("b", "return await loadScript('space:test/a.js');")], "return await Promise.all([loadScript('space:test/a.js'),loadScript('space:test/b.js')]);"),
        (vec![("a", "return {run: () => loadScript('space:test/b.js')};"), ("b", "return await loadScript('space:test/a.js');")], "const a = await loadScript('space:test/a.js'); return await a.run();"),
    ] {
        let error = run(&JsEngine::new(), inline, Provider::new(&files)).await.unwrap_err().to_string();
        assert!(error.contains("dependency_cycle:"), "{error}");
        assert!(error.contains("space:test/a.js ->"), "{error}");
    }
}

#[tokio::test]
async fn original_file_lines_survive_syntax_initialization_and_exported_errors() {
    for (body, inline) in [
        (
            "\n\n\n\n\n\nconst = ;",
            "return await loadScript('space:test/a.js');",
        ),
        (
            "\n\n\n\n\n\nthrow new Error('init');",
            "return await loadScript('space:test/a.js');",
        ),
        (
            "return {run: async () => {\n\n\n\n\n\nthrow new Error('export'); }};",
            "const a = await loadScript('space:test/a.js'); return await a.run();",
        ),
    ] {
        let error = run(&JsEngine::new(), inline, Provider::new(&[("a", body)]))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("space:test/a.js:7"), "{error}");
    }
}

#[tokio::test]
async fn initialization_failures_are_cached_and_audited() {
    let provider = Provider::new(&[("a", "console.log('once'); throw new Error('failure');")]);
    let audit: SharedScriptAudit = Arc::default();
    let result = JsEngine::new().execute_with_sources("for (let i=0;i<2;i++) {try {await loadScript('space:test/a.js')} catch(e) {}} return 'caught';", Arc::new(Echo), Duration::from_secs(2), Some(provider.clone()), audit.clone()).await.unwrap();
    assert_eq!(result.value, "caught");
    assert_eq!(provider.reads.load(Ordering::SeqCst), 1);
    assert_eq!(result.console_output, ["once"]);
    let audit = audit.lock().unwrap();
    assert_eq!(audit.sources.len(), 1);
    assert!(audit.sources[0].error.as_ref().unwrap().contains("failure"));
    assert!(!audit.sources[0].initialized);
    assert!(!serde_json::to_string(&*audit)
        .unwrap()
        .contains("console.log"));
}

#[tokio::test]
async fn cache_hits_reuse_access_but_new_reads_check_permissions() {
    let mut provider = Provider::new(&[("a", "return {};"), ("b", "return {};")]);
    Arc::get_mut(&mut provider).unwrap().revoke_after = 1;
    let result = run(
        &JsEngine::new(),
        "const a = await loadScript('space:test/a.js'); const same = a === await loadScript('space:test/a.js'); let denied; try {await loadScript('space:test/b.js')} catch(e) {denied=String(e)} return {same,denied};",
        provider.clone(),
    ).await.unwrap();
    assert_eq!(result.value["same"], true);
    assert!(result.value["denied"]
        .as_str()
        .unwrap()
        .contains("access_denied"));
    assert_eq!(provider.reads.load(Ordering::SeqCst), 2);
    let error = run(
        &JsEngine::new(),
        "return await loadScript('space:test/a.js');",
        provider.clone(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("access_denied"));
    assert_eq!(provider.reads.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn loader_budgets_cannot_be_caught_in_javascript() {
    for (limits, body, expected) in [
        (ScriptLimits {max_loads: 1, ..Default::default()}, "try {await loadScript('space:test/a.js');await loadScript('space:test/a.js')} catch(e) {} return 'escaped';", "load_limit"),
        (ScriptLimits {max_file_bytes: 1, ..Default::default()}, "try {await loadScript('space:test/a.js')} catch(e) {} return 'escaped';", "size_limit"),
        (ScriptLimits {max_total_bytes: 15, ..Default::default()}, "await Promise.allSettled([loadScript('space:test/a.js'), loadScript('space:test/b.js')]); return 'escaped';", "size_limit"),
        (ScriptLimits {max_dependency_depth: 1, ..Default::default()}, "try {await loadScript('space:test/deep.js')} catch(e) {} return 'escaped';", "depth_limit"),
    ] {
        let provider = Provider::new(&[("a", "return {x:1};"), ("b", "return {x:2};"), ("deep", "return await loadScript('space:test/a.js');")]);
        let error = run(&JsEngine::new().with_script_limits(limits), body, provider).await.unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
    }
}

#[tokio::test]
async fn source_reads_and_initialization_share_the_deadline() {
    let mut provider = Provider::new(&[("a", "return {};")]);
    Arc::get_mut(&mut provider).unwrap().delay = Duration::from_secs(1);
    let error = JsEngine::new()
        .execute_with_sources(
            "return await loadScript('space:test/a.js');",
            Arc::new(Echo),
            Duration::from_millis(20),
            Some(provider),
            Arc::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, JsEngineError::Timeout(_)), "{error}");
    let provider = Provider::new(&[("a", "try { while(true){} } catch(e) {} return 'escaped';")]);
    let error = JsEngine::new()
        .execute_with_sources(
            "return await loadScript('space:test/a.js');",
            Arc::new(Echo),
            Duration::from_millis(20),
            Some(provider),
            Arc::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, JsEngineError::Timeout(_)), "{error}");
}

#[tokio::test]
async fn memory_limit_applies_to_compilation_and_exported_functions() {
    for body in [
        format!("return [{}];", "123456789,".repeat(20_000)),
        "return { run: () => { const a=[]; try {while(true) a.push(new Array(1000).fill('x'));} catch(e) {} return 'escaped'; }};".into(),
    ] {
        let provider = Provider::new(&[("a", &body)]);
        let result = run(&JsEngine::new().with_memory_limit(512 * 1024), "const a = await loadScript('space:test/a.js'); return a.run ? a.run() : a;", provider).await;
        assert!(result.is_err(), "memory termination was caught: {result:?}");
    }
}

#[tokio::test]
async fn stack_limit_applies_to_loaded_functions() {
    let provider = Provider::new(&[(
        "a",
        "function recurse() { return recurse() + 1; } return {run: recurse};",
    )]);
    let error = run(
        &JsEngine::new(),
        "const a = await loadScript('space:test/a.js'); try {a.run()} catch(e) {} return 'escaped';",
        provider,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("stack"), "{error}");
}

#[tokio::test]
async fn tool_calls_after_caught_loader_termination_are_not_dispatched() {
    struct Count(AtomicUsize);
    #[async_trait::async_trait]
    impl ToolDispatcher for Count {
        async fn call_tool(
            &self,
            _: &str,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::Value::Null)
        }
    }
    let dispatcher = Arc::new(Count(AtomicUsize::new(0)));
    let result = JsEngine::new().with_script_limits(ScriptLimits {max_loads:0, ..Default::default()})
        .execute_with_sources("try {await loadScript('space:test/a.js')} catch(e) {} try {await tools.mutate({})} catch(e) {} return 1;", dispatcher.clone(), Duration::from_secs(2), Some(Provider::new(&[])), Arc::default()).await;
    assert!(result.is_err());
    assert_eq!(dispatcher.0.load(Ordering::SeqCst), 0);
}
