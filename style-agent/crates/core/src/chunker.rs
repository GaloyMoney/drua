use tree_sitter::Parser;

/// A chunk of code extracted from a Rust source file via tree-sitter.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub content: String,
    /// AST node type: function_item, struct_item, impl_item, enum_item, trait_item,
    /// impl_method, impl_summary, etc.
    pub chunk_type: String,
    /// The primary identifier (function name, struct name, method name, etc.)
    pub entity_name: Option<String>,
    /// For impl blocks / methods: the type being implemented
    pub impl_type: Option<String>,
    /// For trait impls / methods: the trait name
    pub impl_trait: Option<String>,
    pub line_start: usize,
    pub line_end: usize,
    /// Preceding doc comment (/// or //!)
    pub doc_comment: Option<String>,
}

/// Thresholds for splitting large impl blocks into per-method chunks.
const MAX_METHODS_SINGLE_CHUNK: usize = 5;
const MAX_CHARS_SINGLE_CHUNK: usize = 3200;

/// Metadata about the file a chunk was extracted from.
#[derive(Debug, Clone)]
pub struct ChunkMetadata {
    /// Relative path from repo root
    pub file_path: String,
    pub repo: String,
    /// Derived from file path (e.g. src/foo/bar.rs → foo::bar)
    pub module_path: String,
}

/// Node kinds we extract as top-level chunks.
const CHUNK_NODE_KINDS: &[&str] = &[
    "function_item",
    "struct_item",
    "enum_item",
    "impl_item",
    "trait_item",
    "type_item",
    "const_item",
    "static_item",
    "macro_definition",
    "macro_invocation",
];

/// Parse a Rust source file and extract code chunks.
pub fn chunk_file(source: &str) -> Vec<Chunk> {
    let mut parser = Parser::new();
    let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
    parser
        .set_language(&language)
        .expect("failed to set Rust language");

    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let root = tree.root_node();
    let source_bytes = source.as_bytes();
    let mut chunks = Vec::new();
    let mut cursor = root.walk();

    for child in root.named_children(&mut cursor) {
        collect_chunks(child, source_bytes, source, &mut chunks);
    }

    chunks
}

fn collect_chunks(
    node: tree_sitter::Node<'_>,
    source_bytes: &[u8],
    source: &str,
    chunks: &mut Vec<Chunk>,
) {
    let kind = node.kind();

    if !CHUNK_NODE_KINDS.contains(&kind) {
        return;
    }

    if kind == "impl_item" {
        collect_impl_chunks(node, source_bytes, source, chunks);
        return;
    }

    let node_content = node.utf8_text(source_bytes).unwrap_or_default();
    let entity_name = extract_entity_name(&node, source_bytes);
    let doc_comment = extract_doc_comment(&node, source);
    let attrs = extract_preceding_attributes(&node, source);

    // tree-sitter rows are 0-based; convert to 1-based line numbers
    let line_end = node.end_position().row + 1;
    let (content, line_start) = if let Some((ref attr_block, first_attr_row)) = attrs {
        let combined = format!("{attr_block}\n{node_content}");
        (combined, first_attr_row + 1)
    } else {
        (node_content.to_string(), node.start_position().row + 1)
    };

    chunks.push(Chunk {
        content,
        chunk_type: kind.to_string(),
        entity_name,
        impl_type: None,
        impl_trait: None,
        line_start,
        line_end,
        doc_comment,
    });
}

/// Handle impl blocks: keep small ones as a single chunk, split large ones into per-method chunks.
fn collect_impl_chunks(
    node: tree_sitter::Node<'_>,
    source_bytes: &[u8],
    source: &str,
    chunks: &mut Vec<Chunk>,
) {
    let (impl_type, impl_trait) = extract_impl_info(&node, source_bytes);
    let doc_comment = extract_doc_comment(&node, source);
    let attrs = extract_preceding_attributes(&node, source);

    let methods = extract_methods(&node, source_bytes);
    let node_content = node.utf8_text(source_bytes).unwrap_or_default();

    let (content, first_attr_row) = if let Some((ref attr_block, first_row)) = attrs {
        let combined = format!("{attr_block}\n{node_content}");
        (combined, Some(first_row))
    } else {
        (node_content.to_string(), None)
    };

    let is_large =
        methods.len() > MAX_METHODS_SINGLE_CHUNK || content.len() > MAX_CHARS_SINGLE_CHUNK;

    if !is_large || methods.is_empty() {
        // Small impl block — keep as single chunk (original behavior)
        let line_start = first_attr_row.map_or(node.start_position().row + 1, |r| r + 1);
        let line_end = node.end_position().row + 1;

        chunks.push(Chunk {
            content,
            chunk_type: "impl_item".to_string(),
            entity_name: impl_type.clone(),
            impl_type,
            impl_trait,
            line_start,
            line_end,
            doc_comment,
        });
        return;
    }

    // Large impl block — split into method chunks + summary

    let context_header = build_impl_context_header(impl_type.as_deref(), impl_trait.as_deref());
    let method_names: Vec<&str> = methods.iter().map(|m| m.name.as_str()).collect();

    // Emit per-method chunks
    for method in &methods {
        let method_content = format!("{context_header}\n{}", method.source);
        let line_start = method.line_start;
        let line_end = method.line_end;

        chunks.push(Chunk {
            content: method_content,
            chunk_type: "impl_method".to_string(),
            entity_name: Some(method.name.clone()),
            impl_type: impl_type.clone(),
            impl_trait: impl_trait.clone(),
            line_start,
            line_end,
            doc_comment: method.doc_comment.clone(),
        });
    }

    // Emit summary chunk with signatures only
    let summary_content = build_impl_summary(
        impl_type.as_deref(),
        impl_trait.as_deref(),
        &method_names,
        &methods,
    );
    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    chunks.push(Chunk {
        content: summary_content,
        chunk_type: "impl_summary".to_string(),
        entity_name: impl_type.clone(),
        impl_type,
        impl_trait,
        line_start,
        line_end,
        doc_comment,
    });
}

/// Information about a single method inside an impl block.
struct MethodInfo {
    name: String,
    source: String,
    signature: String,
    line_start: usize,
    line_end: usize,
    doc_comment: Option<String>,
}

/// Extract methods (function_item nodes) from an impl block's declaration_list.
fn extract_methods(node: &tree_sitter::Node<'_>, source: &[u8]) -> Vec<MethodInfo> {
    let decl_list = match node.child_by_field_name("body") {
        Some(dl) => dl,
        None => return Vec::new(),
    };

    let mut methods = Vec::new();
    let mut cursor = decl_list.walk();

    for child in decl_list.named_children(&mut cursor) {
        if child.kind() != "function_item" {
            continue;
        }

        let name = child
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source).ok())
            .unwrap_or("")
            .to_string();

        let node_text = child.utf8_text(source).unwrap_or_default().to_string();
        let signature = extract_fn_signature(&child, source);
        let line_end = child.end_position().row + 1;

        // Collect doc comments and preceding attributes for the method
        let full_source = std::str::from_utf8(source).unwrap_or("");
        let doc_comment = extract_doc_comment(&child, full_source);
        let attrs = extract_preceding_attributes(&child, full_source);

        let (src, line_start) = if let Some((attr_block, first_attr_row)) = attrs {
            let combined = format!("{attr_block}\n{node_text}");
            (combined, first_attr_row + 1)
        } else {
            (node_text, child.start_position().row + 1)
        };

        methods.push(MethodInfo {
            name,
            source: src,
            signature,
            line_start,
            line_end,
            doc_comment,
        });
    }

    methods
}

/// Extract a function signature (everything before the body block).
fn extract_fn_signature(node: &tree_sitter::Node<'_>, source: &[u8]) -> String {
    // The body is a "block" child. Signature is everything before it.
    if let Some(body) = node.child_by_field_name("body") {
        let sig_start = node.start_byte();
        let sig_end = body.start_byte();
        let sig_bytes = &source[sig_start..sig_end];
        let sig = std::str::from_utf8(sig_bytes).unwrap_or("").trim_end();
        return sig.to_string();
    }

    // Fallback: the whole node text (shouldn't happen for function_item)
    node.utf8_text(source).unwrap_or_default().to_string()
}

/// Build the "// impl Type" or "// impl Trait for Type" context header.
fn build_impl_context_header(impl_type: Option<&str>, impl_trait: Option<&str>) -> String {
    match (impl_type, impl_trait) {
        (Some(ty), Some(tr)) => format!("// impl {tr} for {ty}"),
        (Some(ty), None) => format!("// impl {ty}"),
        _ => "// impl".to_string(),
    }
}

/// Build a summary chunk with the impl header and all method signatures.
fn build_impl_summary(
    impl_type: Option<&str>,
    impl_trait: Option<&str>,
    _method_names: &[&str],
    methods: &[MethodInfo],
) -> String {
    let header = match (impl_type, impl_trait) {
        (Some(ty), Some(tr)) => format!("impl {tr} for {ty}"),
        (Some(ty), None) => format!("impl {ty}"),
        _ => "impl".to_string(),
    };

    let mut out = format!("{header} {{\n");
    for method in methods {
        out.push_str("    ");
        out.push_str(&method.signature);
        out.push_str(" { ... }\n");
    }
    out.push('}');
    out
}

/// Extract the primary name from a node (first `identifier` or `type_identifier` child).
fn extract_entity_name(node: &tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    // For most items the name lives in the "name" field
    if let Some(name_node) = node.child_by_field_name("name") {
        return name_node.utf8_text(source).ok().map(|s| s.to_string());
    }

    // Fallback: first identifier / type_identifier child
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let k = child.kind();
        if k == "identifier" || k == "type_identifier" {
            return child.utf8_text(source).ok().map(|s| s.to_string());
        }
    }
    None
}

/// For `impl_item` nodes, extract the type being implemented and the optional trait.
fn extract_impl_info(
    node: &tree_sitter::Node<'_>,
    source: &[u8],
) -> (Option<String>, Option<String>) {
    // impl_item children vary:
    //   impl Foo { ... }                    → type field = Foo
    //   impl Trait for Foo { ... }          → trait field = Trait, type field = Foo
    let impl_type = node
        .child_by_field_name("type")
        .and_then(|n| first_type_identifier(&n, source));

    let impl_trait = node
        .child_by_field_name("trait")
        .and_then(|n| first_type_identifier(&n, source));

    (impl_type, impl_trait)
}

/// Recursively find the first type_identifier text inside a node.
fn first_type_identifier(node: &tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind() == "type_identifier" {
        return node.utf8_text(source).ok().map(|s| s.to_string());
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(name) = first_type_identifier(&child, source) {
            return Some(name);
        }
    }
    None
}

/// Collect preceding `///` or `//!` doc-comment lines immediately above the node.
fn extract_doc_comment(node: &tree_sitter::Node<'_>, source: &str) -> Option<String> {
    let start_row = node.start_position().row;
    if start_row == 0 {
        return None;
    }

    let lines: Vec<&str> = source.lines().collect();
    let mut doc_lines: Vec<&str> = Vec::new();

    // Walk backwards from the line before the node
    let mut row = start_row;
    while row > 0 {
        row -= 1;
        let line = lines.get(row).map(|l| l.trim()).unwrap_or("");
        if line.starts_with("///") || line.starts_with("//!") {
            doc_lines.push(line);
        } else if line.starts_with("#[") || line.starts_with("#![") {
            // Skip attributes between doc comments and the item
            continue;
        } else {
            break;
        }
    }

    if doc_lines.is_empty() {
        return None;
    }

    doc_lines.reverse();
    Some(doc_lines.join("\n"))
}

/// Collect preceding `#[...]` attribute nodes immediately above the node
/// by walking tree-sitter's sibling chain. This correctly handles multi-line
/// attributes like `#[es_repo(\n    entity = "...",\n)]`.
///
/// Returns `(attribute_text, first_attr_start_row)` where the row is 0-based.
fn extract_preceding_attributes(
    node: &tree_sitter::Node<'_>,
    source: &str,
) -> Option<(String, usize)> {
    let source_bytes = source.as_bytes();
    let mut attr_nodes = Vec::new();
    let mut current = *node;

    while let Some(prev) = current.prev_named_sibling() {
        if prev.kind() == "attribute_item" {
            attr_nodes.push(prev);
            current = prev;
        } else {
            break;
        }
    }

    if attr_nodes.is_empty() {
        return None;
    }

    attr_nodes.reverse();
    let first_row = attr_nodes[0].start_position().row;
    let attr_text: Vec<&str> = attr_nodes
        .iter()
        .map(|n| n.utf8_text(source_bytes).unwrap_or(""))
        .collect();
    Some((attr_text.join("\n"), first_row))
}

/// Derive a Rust module path from a file path relative to a repo root.
///
/// Examples:
/// - `src/foo/bar.rs` → `foo::bar`
/// - `src/foo/mod.rs` → `foo`
/// - `src/lib.rs` → (empty string)
pub fn module_path_from_file(file_path: &str) -> String {
    let p = file_path
        .strip_prefix("src/")
        .unwrap_or(file_path)
        .strip_suffix(".rs")
        .unwrap_or(file_path);

    let p = p.strip_suffix("/mod").unwrap_or(p);

    if p == "lib" || p == "main" {
        return String::new();
    }

    p.replace('/', "::")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_function() {
        let src = r#"
/// Add two numbers.
fn add(a: i32, b: i32) -> i32 {
    a + b
}
"#;
        let chunks = chunk_file(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, "function_item");
        assert_eq!(chunks[0].entity_name.as_deref(), Some("add"));
        assert!(chunks[0].doc_comment.is_some());
        assert!(chunks[0].doc_comment.as_ref().unwrap().contains("Add two"));
    }

    #[test]
    fn chunk_struct_and_small_impl() {
        let src = r#"
struct Foo {
    x: i32,
}

impl Foo {
    fn new() -> Self {
        Foo { x: 0 }
    }

    fn x(&self) -> i32 {
        self.x
    }

    fn set_x(&mut self, val: i32) {
        self.x = val;
    }
}
"#;
        let chunks = chunk_file(src);
        // Small impl (3 methods, under char limit) stays as single chunk
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chunk_type, "struct_item");
        assert_eq!(chunks[0].entity_name.as_deref(), Some("Foo"));
        assert_eq!(chunks[1].chunk_type, "impl_item");
        assert_eq!(chunks[1].impl_type.as_deref(), Some("Foo"));
        assert!(chunks[1].impl_trait.is_none());
    }

    #[test]
    fn chunk_trait_impl_small() {
        let src = r#"
impl Display for Foo {
    fn fmt(&self, f: &mut Formatter) -> Result {
        write!(f, "Foo")
    }
}
"#;
        let chunks = chunk_file(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, "impl_item");
        assert_eq!(chunks[0].impl_type.as_deref(), Some("Foo"));
        assert_eq!(chunks[0].impl_trait.as_deref(), Some("Display"));
    }

    #[test]
    fn large_impl_splits_into_methods() {
        let src = r#"
struct Bar;

impl Bar {
    fn method_a(&self) -> i32 { 1 }
    fn method_b(&self) -> i32 { 2 }
    fn method_c(&self) -> i32 { 3 }
    fn method_d(&self) -> i32 { 4 }
    fn method_e(&self) -> i32 { 5 }
    fn method_f(&self) -> i32 { 6 }
    fn method_g(&self) -> i32 { 7 }
    fn method_h(&self) -> i32 { 8 }
}
"#;
        let chunks = chunk_file(src);

        // struct + 8 method chunks + 1 summary = 10
        assert_eq!(chunks.len(), 10);
        assert_eq!(chunks[0].chunk_type, "struct_item");

        // Methods are impl_method type
        for chunk in &chunks[1..=8] {
            assert_eq!(chunk.chunk_type, "impl_method");
            assert_eq!(chunk.impl_type.as_deref(), Some("Bar"));
            assert!(chunk.impl_trait.is_none());
        }
        assert_eq!(chunks[1].entity_name.as_deref(), Some("method_a"));
        assert_eq!(chunks[8].entity_name.as_deref(), Some("method_h"));

        // Last chunk is the summary
        let summary = &chunks[9];
        assert_eq!(summary.chunk_type, "impl_summary");
        assert_eq!(summary.impl_type.as_deref(), Some("Bar"));
        assert!(summary.content.contains("impl Bar {"));
        assert!(summary.content.contains("method_a"));
        assert!(summary.content.contains("method_h"));
        assert!(summary.content.contains("{ ... }"));
    }

    #[test]
    fn method_chunks_have_context_header() {
        let src = r#"
impl Baz {
    fn alpha(&self) -> i32 { 1 }
    fn beta(&self) -> i32 { 2 }
    fn gamma(&self) -> i32 { 3 }
    fn delta(&self) -> i32 { 4 }
    fn epsilon(&self) -> i32 { 5 }
    fn zeta(&self) -> i32 { 6 }
}
"#;
        let chunks = chunk_file(src);

        // 6 methods + 1 summary = 7
        assert_eq!(chunks.len(), 7);

        // Each method chunk should start with context header
        for chunk in &chunks[..6] {
            assert_eq!(chunk.chunk_type, "impl_method");
            assert!(
                chunk.content.starts_with("// impl Baz\n"),
                "Expected context header, got: {}",
                &chunk.content[..chunk.content.len().min(40)]
            );
        }
    }

    #[test]
    fn trait_impl_context_header_format() {
        let src = r#"
impl Display for Widget {
    fn fmt(&self, f: &mut Formatter) -> Result { write!(f, "w") }
    fn other_a(&self) -> i32 { 1 }
    fn other_b(&self) -> i32 { 2 }
    fn other_c(&self) -> i32 { 3 }
    fn other_d(&self) -> i32 { 4 }
    fn other_e(&self) -> i32 { 5 }
}
"#;
        let chunks = chunk_file(src);

        // 6 methods + 1 summary = 7
        assert_eq!(chunks.len(), 7);

        // Method chunks should have trait impl context header
        for chunk in &chunks[..6] {
            assert_eq!(chunk.chunk_type, "impl_method");
            assert!(
                chunk.content.starts_with("// impl Display for Widget\n"),
                "Expected trait impl context header, got: {}",
                &chunk.content[..chunk.content.len().min(50)]
            );
            assert_eq!(chunk.impl_type.as_deref(), Some("Widget"));
            assert_eq!(chunk.impl_trait.as_deref(), Some("Display"));
        }

        // Summary chunk
        let summary = chunks.last().unwrap();
        assert_eq!(summary.chunk_type, "impl_summary");
        assert!(summary.content.contains("impl Display for Widget {"));
    }

    #[test]
    fn small_impl_within_thresholds_stays_single() {
        // 5 methods exactly, short bodies — should stay as single chunk
        let src = r#"
impl Small {
    fn a(&self) {}
    fn b(&self) {}
    fn c(&self) {}
    fn d(&self) {}
    fn e(&self) {}
}
"#;
        let chunks = chunk_file(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, "impl_item");
        assert_eq!(chunks[0].impl_type.as_deref(), Some("Small"));
    }

    #[test]
    fn impl_summary_contains_all_signatures() {
        let src = r#"
impl Svc {
    pub fn new(db: &str) -> Self { todo!() }
    pub fn get(&self, id: u64) -> Option<String> { todo!() }
    fn internal_helper(&self) { todo!() }
    pub fn update(&mut self, id: u64, val: &str) -> bool { todo!() }
    pub fn delete(&mut self, id: u64) -> bool { todo!() }
    pub fn list(&self) -> Vec<String> { todo!() }
}
"#;
        let chunks = chunk_file(src);

        let summary = chunks.iter().find(|c| c.chunk_type == "impl_summary");
        assert!(summary.is_some(), "Expected an impl_summary chunk");
        let summary = summary.unwrap();

        assert!(summary.content.contains("pub fn new("));
        assert!(summary.content.contains("pub fn get("));
        assert!(summary.content.contains("fn internal_helper("));
        assert!(summary.content.contains("pub fn update("));
        assert!(summary.content.contains("pub fn delete("));
        assert!(summary.content.contains("pub fn list("));
    }

    #[test]
    fn module_path_derivation() {
        assert_eq!(module_path_from_file("src/foo/bar.rs"), "foo::bar");
        assert_eq!(module_path_from_file("src/foo/mod.rs"), "foo");
        assert_eq!(module_path_from_file("src/lib.rs"), "");
        assert_eq!(module_path_from_file("src/main.rs"), "");
    }

    #[test]
    fn struct_includes_derive_attributes() {
        let src = r#"
#[derive(Debug, Clone, EsEntity)]
#[serde(tag = "type", rename_all = "snake_case")]
pub struct MyEntity {
    id: String,
}
"#;
        let chunks = chunk_file(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, "struct_item");
        assert!(
            chunks[0]
                .content
                .contains("#[derive(Debug, Clone, EsEntity)]"),
            "Expected derive attribute in content, got:\n{}",
            chunks[0].content
        );
        assert!(
            chunks[0].content.contains("#[serde("),
            "Expected serde attribute in content"
        );
        // line_start should cover the attributes
        assert_eq!(chunks[0].line_start, 2);
    }

    #[test]
    fn enum_includes_derive_attributes() {
        let src = r#"
#[derive(EsEvent)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MyEvent {
    Created,
    Updated,
}
"#;
        let chunks = chunk_file(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, "enum_item");
        assert!(chunks[0].content.contains("#[derive(EsEvent)]"));
        assert!(chunks[0].content.contains("#[serde("));
    }

    #[test]
    fn doc_comment_and_attributes_both_captured() {
        let src = r#"
/// My entity doc
#[derive(Debug, Clone)]
pub struct Documented {
    x: i32,
}
"#;
        let chunks = chunk_file(src);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].doc_comment.is_some());
        assert!(chunks[0]
            .doc_comment
            .as_ref()
            .unwrap()
            .contains("My entity doc"));
        assert!(
            chunks[0].content.contains("#[derive(Debug, Clone)]"),
            "Expected derive in content, got:\n{}",
            chunks[0].content
        );
        // Doc comment is NOT included in attributes (it's in doc_comment field)
        assert!(!chunks[0].content.contains("/// My entity doc"));
    }

    #[test]
    fn struct_with_multiline_attribute_includes_all_attrs() {
        let src = r#"
#[derive(EsRepo)]
#[es_repo(
    entity = "Customer",
    columns(
        party_id(ty = "PartyId", list_by),
    ),
    tbl_prefix = "core",
)]
pub struct CustomerRepo {
    pool: PgPool,
}
"#;
        let chunks = chunk_file(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, "struct_item");
        assert!(
            chunks[0].content.contains("#[derive(EsRepo)]"),
            "Expected #[derive(EsRepo)] in content, got:\n{}",
            chunks[0].content
        );
        assert!(
            chunks[0].content.contains("#[es_repo("),
            "Expected #[es_repo( in content, got:\n{}",
            chunks[0].content
        );
    }

    #[test]
    fn no_attributes_no_change() {
        let src = r#"
pub struct Plain {
    x: i32,
}
"#;
        let chunks = chunk_file(src);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].content.starts_with("pub struct Plain"));
    }

    #[test]
    fn function_includes_instrument_attribute() {
        let src = r#"
#[instrument(name = "my_crate.my_module.do_thing", skip_all)]
pub fn do_thing(input: &str) -> Result<()> {
    Ok(())
}
"#;
        let chunks = chunk_file(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, "function_item");
        assert!(
            chunks[0].content.contains("#[instrument("),
            "Expected instrument attribute in content, got:\n{}",
            chunks[0].content
        );
    }

    #[test]
    fn small_impl_methods_include_instrument_attributes() {
        let src = r#"
impl MyService {
    #[instrument(name = "my_service.create", skip_all)]
    pub fn create(&self, input: &str) -> Result<()> {
        Ok(())
    }

    #[instrument(name = "my_service.find", skip_all)]
    pub fn find(&self, id: u64) -> Option<String> {
        None
    }
}
"#;
        let chunks = chunk_file(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, "impl_item");
        // Small impl keeps the full block — attributes are part of the raw source inside the body
        assert!(
            chunks[0].content.contains("#[instrument("),
            "Expected instrument attribute in small impl content, got:\n{}",
            chunks[0].content
        );
    }

    #[test]
    fn large_impl_method_chunks_include_instrument_attributes() {
        let src = r#"
impl BigService {
    #[instrument(name = "big_service.method_a", skip_all)]
    pub fn method_a(&self) -> i32 { 1 }

    #[instrument(name = "big_service.method_b", skip_all)]
    pub fn method_b(&self) -> i32 { 2 }

    #[instrument(name = "big_service.method_c", skip_all)]
    pub fn method_c(&self) -> i32 { 3 }

    pub fn method_d(&self) -> i32 { 4 }

    #[instrument(name = "big_service.method_e", skip_all)]
    pub fn method_e(&self) -> i32 { 5 }

    #[instrument(name = "big_service.method_f", skip_all)]
    pub fn method_f(&self) -> i32 { 6 }
}
"#;
        let chunks = chunk_file(src);

        // 6 methods + 1 summary = 7
        assert_eq!(chunks.len(), 7);

        // Methods with #[instrument] should include it in their content
        let method_a = chunks
            .iter()
            .find(|c| c.entity_name.as_deref() == Some("method_a"))
            .unwrap();
        assert!(
            method_a.content.contains("#[instrument("),
            "Expected instrument attribute on method_a, got:\n{}",
            method_a.content
        );

        let method_b = chunks
            .iter()
            .find(|c| c.entity_name.as_deref() == Some("method_b"))
            .unwrap();
        assert!(
            method_b.content.contains("#[instrument("),
            "Expected instrument attribute on method_b, got:\n{}",
            method_b.content
        );

        // method_d has no attribute — should not contain #[instrument
        let method_d = chunks
            .iter()
            .find(|c| c.entity_name.as_deref() == Some("method_d"))
            .unwrap();
        assert!(
            !method_d.content.contains("#[instrument("),
            "method_d should NOT have instrument attribute, got:\n{}",
            method_d.content
        );
    }

    #[test]
    fn impl_with_attributes_includes_them() {
        let src = r#"
#[async_trait]
impl MyTrait for Foo {
    async fn do_thing(&self) {}
}
"#;
        let chunks = chunk_file(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, "impl_item");
        assert!(
            chunks[0].content.contains("#[async_trait]"),
            "Expected async_trait attribute in content, got:\n{}",
            chunks[0].content
        );
    }
}
