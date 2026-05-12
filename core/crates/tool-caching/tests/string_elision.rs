use drua_tool_caching::{
    QueryStructure, StringSummarizerChain, ToolCachingConfig, ToolInvocationId, Walker,
};

/// Multi-line content goes through line-mode: head + tail are line
/// counts, recovery uses `mode: "lines"`.
#[test]
fn multi_line_string_uses_line_mode() {
    let config = ToolCachingConfig {
        generic_threshold_bytes: 256,
        ..ToolCachingConfig::default()
    };
    let walker = Walker::new(std::sync::Arc::new(StringSummarizerChain::new()), &config);

    let lines: Vec<String> = (0..200).map(|i| format!("line {i}")).collect();
    let big = lines.join("\n");
    let q = QueryStructure::new(&big);
    let invocation_id = ToolInvocationId::new();
    let summary = walker.summarize(&q, invocation_id, "test");

    assert_eq!(summary.elided_paths.len(), 1);
    let entry = &summary.elided_paths[0];
    assert_eq!(entry.path, "$");
    assert_eq!(entry.bytes as usize, big.len());
    assert_eq!(entry.lines, Some(200));
    assert_eq!(entry.length, None);
    let query = &entry.recover["args_template"]["query"];
    assert_eq!(query["mode"], "lines");
    assert!(query["offset"].as_u64().is_some());
    assert!(query["len"].as_u64().is_some());

    let kept = summary.summary.as_str().unwrap();
    assert!(kept.contains("<head lines=\""));
    assert!(kept.contains("<bulk-elided original-lines=\""));
    assert!(kept.contains("<tail lines=\""));
    assert!(
        !kept.contains("bytes=\""),
        "line-mode markers must not mix in byte attrs"
    );
}

/// Single-line content (no usable newline split) falls back to byte-mode.
#[test]
fn single_line_string_falls_back_to_byte_mode() {
    let config = ToolCachingConfig {
        generic_threshold_bytes: 1024,
        ..ToolCachingConfig::default()
    };
    let walker = Walker::new(std::sync::Arc::new(StringSummarizerChain::new()), &config);

    let big = "x".repeat(5000);
    let q = QueryStructure::new(&big);
    let summary = walker.summarize(&q, ToolInvocationId::new(), "test");

    assert_eq!(summary.elided_paths.len(), 1);
    let entry = &summary.elided_paths[0];
    assert_eq!(entry.lines, None);
    let query = &entry.recover["args_template"]["query"];
    assert_eq!(query["mode"], "range");

    let kept = summary.summary.as_str().unwrap();
    assert!(kept.contains("<head bytes=\""));
    assert!(kept.contains("<bulk-elided original-bytes=\""));
    assert!(kept.contains("<tail bytes=\""));
    assert!(
        !kept.contains("lines=\""),
        "byte-mode markers must not mix in line attrs"
    );
}

#[test]
fn root_string_below_threshold_passes_through() {
    let config = ToolCachingConfig {
        generic_threshold_bytes: 1024,
        ..ToolCachingConfig::default()
    };
    let walker = Walker::new(std::sync::Arc::new(StringSummarizerChain::new()), &config);

    let small = "just a short line".to_string();
    let q = QueryStructure::new(&small);
    let summary = walker.summarize(&q, ToolInvocationId::new(), "test");

    assert!(summary.elided_paths.is_empty());
    assert_eq!(summary.summary.as_str(), Some(small.as_str()));
}
