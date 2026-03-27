use crate::chunker::Chunk;
use tree_sitter::Parser;

/// Supported JS/TS file variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsDialect {
    /// Plain JavaScript (`.js`)
    JavaScript,
    /// JavaScript with JSX (`.jsx`)
    Jsx,
    /// TypeScript (`.ts`)
    TypeScript,
    /// TypeScript with JSX (`.tsx`)
    Tsx,
}

impl JsDialect {
    /// Detect dialect from a file extension (including the dot).
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            ".js" => Some(Self::JavaScript),
            ".jsx" => Some(Self::Jsx),
            ".ts" => Some(Self::TypeScript),
            ".tsx" => Some(Self::Tsx),
            _ => None,
        }
    }

    /// Language identifier for metadata.
    pub fn language_id(self) -> &'static str {
        match self {
            Self::JavaScript | Self::Jsx => "javascript",
            Self::TypeScript | Self::Tsx => "typescript",
        }
    }

    fn tree_sitter_language(self) -> tree_sitter::Language {
        match self {
            // tree-sitter-javascript handles both .js and .jsx
            Self::JavaScript | Self::Jsx => tree_sitter_javascript::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        }
    }
}

/// Top-level AST node kinds to extract as chunks.
const CHUNK_NODE_KINDS: &[&str] = &[
    "function_declaration",
    "class_declaration",
    "lexical_declaration", // `const x = () => {}` — arrow functions and constants
    "variable_declaration", // `var x = ...` — legacy
    "export_statement",
    // TypeScript-specific
    "interface_declaration",
    "type_alias_declaration",
    "enum_declaration",
];

/// Thresholds for splitting large classes into per-method chunks.
const MAX_METHODS_SINGLE_CHUNK: usize = 5;
const MAX_CHARS_SINGLE_CHUNK: usize = 3200;

/// Parse a JavaScript/TypeScript source file and extract code chunks.
///
/// The `dialect` parameter selects the correct tree-sitter grammar.
pub fn chunk_js_file(source: &str, dialect: JsDialect) -> Vec<Chunk> {
    let mut parser = Parser::new();
    let language = dialect.tree_sitter_language();
    parser
        .set_language(&language)
        .expect("failed to set JS/TS language");

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

    // Unwrap export_statement → extract the inner declaration
    if kind == "export_statement" {
        collect_export_chunks(node, source_bytes, source, chunks);
        return;
    }

    // Arrow function or const binding inside lexical/variable declaration
    if kind == "lexical_declaration" || kind == "variable_declaration" {
        collect_declaration_chunks(node, source_bytes, source, chunks);
        return;
    }

    // Class declarations get special splitting treatment
    if kind == "class_declaration" {
        collect_class_chunks(node, source_bytes, source, chunks);
        return;
    }

    // Plain function_declaration, interface, type_alias, enum
    let entity_name = extract_entity_name(&node, source_bytes);
    let doc_comment = extract_jsdoc_comment(&node, source);
    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;
    let content = node.utf8_text(source_bytes).unwrap_or_default().to_string();

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

/// Handle `export_statement` by unwrapping to the inner declaration.
fn collect_export_chunks(
    node: tree_sitter::Node<'_>,
    source_bytes: &[u8],
    source: &str,
    chunks: &mut Vec<Chunk>,
) {
    // Look for an inner declaration node
    let mut cursor = node.walk();
    let mut found_inner = false;

    for child in node.named_children(&mut cursor) {
        let child_kind = child.kind();
        match child_kind {
            "function_declaration" => {
                found_inner = true;
                // Include the full export statement text but classify by inner type
                let entity_name = extract_entity_name(&child, source_bytes);
                let doc_comment = extract_jsdoc_comment(&node, source);
                let content = node.utf8_text(source_bytes).unwrap_or_default().to_string();
                chunks.push(Chunk {
                    content,
                    chunk_type: "function_declaration".to_string(),
                    entity_name,
                    impl_type: None,
                    impl_trait: None,
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                    doc_comment,
                });
            }
            "class_declaration" => {
                found_inner = true;
                // Delegate to class handler but wrap in export text
                collect_class_chunks_with_export(node, child, source_bytes, source, chunks);
            }
            "lexical_declaration" | "variable_declaration" => {
                found_inner = true;
                // Arrow function or const export
                collect_declaration_chunks_with_export(node, child, source_bytes, source, chunks);
            }
            "interface_declaration" | "type_alias_declaration" | "enum_declaration" => {
                found_inner = true;
                let entity_name = extract_entity_name(&child, source_bytes);
                let doc_comment = extract_jsdoc_comment(&node, source);
                let content = node.utf8_text(source_bytes).unwrap_or_default().to_string();
                chunks.push(Chunk {
                    content,
                    chunk_type: child_kind.to_string(),
                    entity_name,
                    impl_type: None,
                    impl_trait: None,
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                    doc_comment,
                });
            }
            _ => {}
        }
    }

    // Bare export (re-export, default export expression, etc.)
    if !found_inner {
        let content = node.utf8_text(source_bytes).unwrap_or_default().to_string();
        // Try to extract a name from `export default <identifier>` patterns
        let entity_name = extract_export_default_name(&node, source_bytes);
        let doc_comment = extract_jsdoc_comment(&node, source);
        chunks.push(Chunk {
            content,
            chunk_type: "export_statement".to_string(),
            entity_name,
            impl_type: None,
            impl_trait: None,
            line_start: node.start_position().row + 1,
            line_end: node.end_position().row + 1,
            doc_comment,
        });
    }
}

/// Handle `lexical_declaration` (const/let) and `variable_declaration` (var).
///
/// Extracts the variable name. If the initializer is an arrow function or
/// function expression, tags the chunk_type as `arrow_function` or
/// `function_declaration` respectively.
fn collect_declaration_chunks(
    node: tree_sitter::Node<'_>,
    source_bytes: &[u8],
    source: &str,
    chunks: &mut Vec<Chunk>,
) {
    let content = node.utf8_text(source_bytes).unwrap_or_default().to_string();
    let doc_comment = extract_jsdoc_comment(&node, source);
    let line_start = node.start_position().row + 1;
    let line_end = node.end_position().row + 1;

    // Walk variable_declarators to find names and initializer types
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            let name = child
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source_bytes).ok())
                .map(|s| s.to_string());

            let chunk_type = match child.child_by_field_name("value") {
                Some(val) if val.kind() == "arrow_function" => "arrow_function",
                Some(val) if val.kind() == "function_expression" => "function_declaration",
                Some(val) if val.kind() == "call_expression" => {
                    // e.g. `const MyContext = React.createContext(null)`
                    // or `const styled = styled.div``
                    "lexical_declaration"
                }
                _ => "lexical_declaration",
            };

            chunks.push(Chunk {
                content: content.clone(),
                chunk_type: chunk_type.to_string(),
                entity_name: name,
                impl_type: None,
                impl_trait: None,
                line_start,
                line_end,
                doc_comment: doc_comment.clone(),
            });
            return;
        }
    }

    // Fallback if no variable_declarator found
    chunks.push(Chunk {
        content,
        chunk_type: node.kind().to_string(),
        entity_name: None,
        impl_type: None,
        impl_trait: None,
        line_start,
        line_end,
        doc_comment,
    });
}

/// Handle exported lexical/variable declarations.
fn collect_declaration_chunks_with_export(
    export_node: tree_sitter::Node<'_>,
    decl_node: tree_sitter::Node<'_>,
    source_bytes: &[u8],
    source: &str,
    chunks: &mut Vec<Chunk>,
) {
    let content = export_node
        .utf8_text(source_bytes)
        .unwrap_or_default()
        .to_string();
    let doc_comment = extract_jsdoc_comment(&export_node, source);
    let line_start = export_node.start_position().row + 1;
    let line_end = export_node.end_position().row + 1;

    let mut cursor = decl_node.walk();
    for child in decl_node.named_children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            let name = child
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source_bytes).ok())
                .map(|s| s.to_string());

            let chunk_type = match child.child_by_field_name("value") {
                Some(val) if val.kind() == "arrow_function" => "arrow_function",
                Some(val) if val.kind() == "function_expression" => "function_declaration",
                _ => "lexical_declaration",
            };

            chunks.push(Chunk {
                content: content.clone(),
                chunk_type: chunk_type.to_string(),
                entity_name: name,
                impl_type: None,
                impl_trait: None,
                line_start,
                line_end,
                doc_comment: doc_comment.clone(),
            });
            return;
        }
    }

    chunks.push(Chunk {
        content,
        chunk_type: "lexical_declaration".to_string(),
        entity_name: None,
        impl_type: None,
        impl_trait: None,
        line_start,
        line_end,
        doc_comment,
    });
}

/// Handle class declarations, splitting large classes into per-method chunks.
fn collect_class_chunks(
    node: tree_sitter::Node<'_>,
    source_bytes: &[u8],
    source: &str,
    chunks: &mut Vec<Chunk>,
) {
    let class_name = extract_entity_name(&node, source_bytes);
    let doc_comment = extract_jsdoc_comment(&node, source);
    let content = node.utf8_text(source_bytes).unwrap_or_default().to_string();
    let methods = extract_class_methods(&node, source_bytes, source);

    let is_large =
        methods.len() > MAX_METHODS_SINGLE_CHUNK || content.len() > MAX_CHARS_SINGLE_CHUNK;

    if !is_large || methods.is_empty() {
        chunks.push(Chunk {
            content,
            chunk_type: "class_declaration".to_string(),
            entity_name: class_name,
            impl_type: None,
            impl_trait: None,
            line_start: node.start_position().row + 1,
            line_end: node.end_position().row + 1,
            doc_comment,
        });
        return;
    }

    // Large class — split into per-method chunks + summary
    let context_header = format!("// class {}", class_name.as_deref().unwrap_or("Anonymous"));

    for method in &methods {
        let method_content = format!("{context_header}\n{}", method.source);
        chunks.push(Chunk {
            content: method_content,
            chunk_type: "method_definition".to_string(),
            entity_name: Some(method.name.clone()),
            impl_type: class_name.clone(),
            impl_trait: None,
            line_start: method.line_start,
            line_end: method.line_end,
            doc_comment: method.doc_comment.clone(),
        });
    }

    // Summary with method signatures
    let summary = build_class_summary(class_name.as_deref(), &methods);
    chunks.push(Chunk {
        content: summary,
        chunk_type: "class_summary".to_string(),
        entity_name: class_name,
        impl_type: None,
        impl_trait: None,
        line_start: node.start_position().row + 1,
        line_end: node.end_position().row + 1,
        doc_comment,
    });
}

/// Handle exported class declarations.
fn collect_class_chunks_with_export(
    export_node: tree_sitter::Node<'_>,
    class_node: tree_sitter::Node<'_>,
    source_bytes: &[u8],
    source: &str,
    chunks: &mut Vec<Chunk>,
) {
    let class_name = extract_entity_name(&class_node, source_bytes);
    let doc_comment = extract_jsdoc_comment(&export_node, source);
    let content = export_node
        .utf8_text(source_bytes)
        .unwrap_or_default()
        .to_string();
    let methods = extract_class_methods(&class_node, source_bytes, source);

    let is_large =
        methods.len() > MAX_METHODS_SINGLE_CHUNK || content.len() > MAX_CHARS_SINGLE_CHUNK;

    if !is_large || methods.is_empty() {
        chunks.push(Chunk {
            content,
            chunk_type: "class_declaration".to_string(),
            entity_name: class_name,
            impl_type: None,
            impl_trait: None,
            line_start: export_node.start_position().row + 1,
            line_end: export_node.end_position().row + 1,
            doc_comment,
        });
        return;
    }

    let context_header = format!("// class {}", class_name.as_deref().unwrap_or("Anonymous"));

    for method in &methods {
        let method_content = format!("{context_header}\n{}", method.source);
        chunks.push(Chunk {
            content: method_content,
            chunk_type: "method_definition".to_string(),
            entity_name: Some(method.name.clone()),
            impl_type: class_name.clone(),
            impl_trait: None,
            line_start: method.line_start,
            line_end: method.line_end,
            doc_comment: method.doc_comment.clone(),
        });
    }

    let summary = build_class_summary(class_name.as_deref(), &methods);
    chunks.push(Chunk {
        content: summary,
        chunk_type: "class_summary".to_string(),
        entity_name: class_name,
        impl_type: None,
        impl_trait: None,
        line_start: export_node.start_position().row + 1,
        line_end: export_node.end_position().row + 1,
        doc_comment,
    });
}

/// Information about a class method.
struct MethodInfo {
    name: String,
    source: String,
    signature: String,
    line_start: usize,
    line_end: usize,
    doc_comment: Option<String>,
}

/// Extract methods from a class body.
fn extract_class_methods(
    class_node: &tree_sitter::Node<'_>,
    source_bytes: &[u8],
    source: &str,
) -> Vec<MethodInfo> {
    let body = match class_node.child_by_field_name("body") {
        Some(b) => b,
        None => return Vec::new(),
    };

    let mut methods = Vec::new();
    let mut cursor = body.walk();

    for child in body.named_children(&mut cursor) {
        let child_kind = child.kind();
        if child_kind != "method_definition" && child_kind != "public_field_definition" {
            continue;
        }

        // Skip property definitions that aren't method-like
        if child_kind == "public_field_definition" {
            // Only include if initializer is a function/arrow
            let has_fn = child
                .child_by_field_name("value")
                .map(|v| v.kind() == "arrow_function" || v.kind() == "function_expression")
                .unwrap_or(false);
            if !has_fn {
                continue;
            }
        }

        let name = child
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source_bytes).ok())
            .unwrap_or("")
            .to_string();

        let node_text = child
            .utf8_text(source_bytes)
            .unwrap_or_default()
            .to_string();
        let signature = extract_method_signature(&child, source_bytes);
        let doc_comment = extract_jsdoc_comment(&child, source);

        methods.push(MethodInfo {
            name,
            source: node_text,
            signature,
            line_start: child.start_position().row + 1,
            line_end: child.end_position().row + 1,
            doc_comment,
        });
    }

    methods
}

/// Extract a method signature (everything before the body block).
fn extract_method_signature(node: &tree_sitter::Node<'_>, source_bytes: &[u8]) -> String {
    if let Some(body) = node.child_by_field_name("body") {
        let sig_start = node.start_byte();
        let sig_end = body.start_byte();
        let sig_bytes = &source_bytes[sig_start..sig_end];
        let sig = std::str::from_utf8(sig_bytes).unwrap_or("").trim_end();
        return sig.to_string();
    }
    node.utf8_text(source_bytes).unwrap_or_default().to_string()
}

/// Build a class summary chunk with all method signatures.
fn build_class_summary(class_name: Option<&str>, methods: &[MethodInfo]) -> String {
    let header = format!("class {}", class_name.unwrap_or("Anonymous"));
    let mut out = format!("{header} {{\n");
    for method in methods {
        out.push_str("  ");
        out.push_str(&method.signature);
        out.push_str(" { ... }\n");
    }
    out.push('}');
    out
}

/// Extract the primary name from a node.
fn extract_entity_name(node: &tree_sitter::Node<'_>, source_bytes: &[u8]) -> Option<String> {
    // Try "name" field first (works for function_declaration, class_declaration,
    // interface_declaration, enum_declaration, type_alias_declaration)
    if let Some(name_node) = node.child_by_field_name("name") {
        return name_node
            .utf8_text(source_bytes)
            .ok()
            .map(|s| s.to_string());
    }

    // Fallback: first identifier child
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "identifier" || child.kind() == "type_identifier" {
            return child.utf8_text(source_bytes).ok().map(|s| s.to_string());
        }
    }
    None
}

/// For `export default <identifier>`, extract the identifier name.
fn extract_export_default_name(
    node: &tree_sitter::Node<'_>,
    source_bytes: &[u8],
) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "identifier" {
            return child.utf8_text(source_bytes).ok().map(|s| s.to_string());
        }
    }
    None
}

/// Collect preceding JSDoc/block comments (`/** ... */`) immediately above the node.
fn extract_jsdoc_comment(node: &tree_sitter::Node<'_>, source: &str) -> Option<String> {
    let start_row = node.start_position().row;
    if start_row == 0 {
        return None;
    }

    let lines: Vec<&str> = source.lines().collect();
    let mut doc_lines: Vec<&str> = Vec::new();
    let mut in_block_comment = false;

    // Walk backwards from the line before the node
    let mut row = start_row;
    while row > 0 {
        row -= 1;
        let line = lines.get(row).map(|l| l.trim()).unwrap_or("");

        if line.is_empty() {
            if !doc_lines.is_empty() {
                break; // Gap between comment and node means it's not attached
            }
            continue;
        }

        // Single-line JSDoc: `/** brief */`
        if line.starts_with("/**") && line.ends_with("*/") && !in_block_comment {
            doc_lines.push(line);
            break;
        }

        // End of block comment (scanning upward)
        if line.ends_with("*/") && !in_block_comment {
            in_block_comment = true;
            doc_lines.push(line);
            continue;
        }

        // Inside block comment
        if in_block_comment {
            doc_lines.push(line);
            if line.starts_with("/**") || line.starts_with("/*") {
                break;
            }
            continue;
        }

        // Single-line `//` comments
        if line.starts_with("//") {
            doc_lines.push(line);
            continue;
        }

        // Decorators (skip them to find comments above)
        if line.starts_with('@') {
            continue;
        }

        break;
    }

    if doc_lines.is_empty() {
        return None;
    }

    doc_lines.reverse();
    Some(doc_lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- basic function extraction ----

    #[test]
    fn chunk_function_declaration() {
        let src = "function add(a, b) {\n  return a + b;\n}\n";
        let chunks = chunk_js_file(src, JsDialect::JavaScript);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, "function_declaration");
        assert_eq!(chunks[0].entity_name.as_deref(), Some("add"));
    }

    #[test]
    fn chunk_exported_function() {
        let src = "export function greet(name) {\n  return `Hello ${name}`;\n}\n";
        let chunks = chunk_js_file(src, JsDialect::JavaScript);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, "function_declaration");
        assert_eq!(chunks[0].entity_name.as_deref(), Some("greet"));
        assert!(chunks[0].content.starts_with("export"));
    }

    #[test]
    fn chunk_arrow_function() {
        let src = "const add = (a, b) => a + b;\n";
        let chunks = chunk_js_file(src, JsDialect::JavaScript);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, "arrow_function");
        assert_eq!(chunks[0].entity_name.as_deref(), Some("add"));
    }

    #[test]
    fn chunk_exported_arrow_function() {
        let src = "export const useAuth = () => {\n  return null;\n};\n";
        let chunks = chunk_js_file(src, JsDialect::JavaScript);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, "arrow_function");
        assert_eq!(chunks[0].entity_name.as_deref(), Some("useAuth"));
        assert!(chunks[0].content.starts_with("export"));
    }

    // ---- class extraction ----

    #[test]
    fn chunk_small_class() {
        let src = r#"class Greeter {
  constructor(name) {
    this.name = name;
  }

  greet() {
    return `Hello ${this.name}`;
  }
}
"#;
        let chunks = chunk_js_file(src, JsDialect::JavaScript);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, "class_declaration");
        assert_eq!(chunks[0].entity_name.as_deref(), Some("Greeter"));
    }

    #[test]
    fn large_class_splits_into_methods() {
        let src = r#"class BigService {
  method_a() { return 1; }
  method_b() { return 2; }
  method_c() { return 3; }
  method_d() { return 4; }
  method_e() { return 5; }
  method_f() { return 6; }
  method_g() { return 7; }
  method_h() { return 8; }
}
"#;
        let chunks = chunk_js_file(src, JsDialect::JavaScript);

        // 8 methods + 1 summary = 9
        assert_eq!(chunks.len(), 9);

        for chunk in &chunks[..8] {
            assert_eq!(chunk.chunk_type, "method_definition");
            assert_eq!(chunk.impl_type.as_deref(), Some("BigService"));
            assert!(
                chunk.content.starts_with("// class BigService\n"),
                "Expected context header, got: {}",
                &chunk.content[..chunk.content.len().min(40)]
            );
        }

        assert_eq!(chunks[0].entity_name.as_deref(), Some("method_a"));
        assert_eq!(chunks[7].entity_name.as_deref(), Some("method_h"));

        let summary = &chunks[8];
        assert_eq!(summary.chunk_type, "class_summary");
        assert!(summary.content.contains("class BigService {"));
        assert!(summary.content.contains("method_a"));
        assert!(summary.content.contains("{ ... }"));
    }

    // ---- TypeScript-specific ----

    #[test]
    fn chunk_interface_declaration() {
        let src = "export interface UserProps {\n  name: string;\n  age: number;\n}\n";
        let chunks = chunk_js_file(src, JsDialect::TypeScript);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, "interface_declaration");
        assert_eq!(chunks[0].entity_name.as_deref(), Some("UserProps"));
    }

    #[test]
    fn chunk_type_alias() {
        let src = "export type UserId = string;\n";
        let chunks = chunk_js_file(src, JsDialect::TypeScript);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, "type_alias_declaration");
        assert_eq!(chunks[0].entity_name.as_deref(), Some("UserId"));
    }

    #[test]
    fn chunk_enum_declaration() {
        let src = "export enum Status {\n  Active,\n  Inactive,\n}\n";
        let chunks = chunk_js_file(src, JsDialect::TypeScript);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, "enum_declaration");
        assert_eq!(chunks[0].entity_name.as_deref(), Some("Status"));
    }

    // ---- JSX / TSX ----

    #[test]
    fn chunk_tsx_component() {
        let src = r#"export function MyComponent({ name }: { name: string }) {
  return <div>{name}</div>;
}
"#;
        let chunks = chunk_js_file(src, JsDialect::Tsx);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, "function_declaration");
        assert_eq!(chunks[0].entity_name.as_deref(), Some("MyComponent"));
    }

    #[test]
    fn chunk_tsx_arrow_component() {
        let src = r#"export const Card = ({ title }: { title: string }) => {
  return <div className="card">{title}</div>;
};
"#;
        let chunks = chunk_js_file(src, JsDialect::Tsx);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, "arrow_function");
        assert_eq!(chunks[0].entity_name.as_deref(), Some("Card"));
    }

    // ---- JSDoc comment extraction ----

    #[test]
    fn extracts_jsdoc_comment() {
        let src = r#"/**
 * Add two numbers.
 * @param a First number
 * @param b Second number
 */
function add(a, b) {
  return a + b;
}
"#;
        let chunks = chunk_js_file(src, JsDialect::JavaScript);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].doc_comment.is_some());
        assert!(chunks[0]
            .doc_comment
            .as_ref()
            .unwrap()
            .contains("Add two numbers"));
    }

    #[test]
    fn extracts_single_line_comment() {
        let src = "// Helper function\nfunction helper() { return 1; }\n";
        let chunks = chunk_js_file(src, JsDialect::JavaScript);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].doc_comment.is_some());
        assert!(chunks[0]
            .doc_comment
            .as_ref()
            .unwrap()
            .contains("Helper function"));
    }

    // ---- line numbers ----

    #[test]
    fn line_numbers_are_one_based() {
        let src = "\nfunction first() { return 1; }\n\nfunction second() { return 2; }\n";
        let chunks = chunk_js_file(src, JsDialect::JavaScript);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].entity_name.as_deref(), Some("first"));
        assert_eq!(chunks[0].line_start, 2);
        assert_eq!(chunks[1].entity_name.as_deref(), Some("second"));
        assert_eq!(chunks[1].line_start, 4);
    }

    // ---- empty / comment-only files ----

    #[test]
    fn empty_file_returns_no_chunks() {
        assert!(chunk_js_file("", JsDialect::JavaScript).is_empty());
        assert!(chunk_js_file("// just a comment\n", JsDialect::JavaScript).is_empty());
    }

    // ---- barrel export ----

    #[test]
    fn barrel_export_produces_chunk() {
        let src = "export { default as Button } from './Button';\n";
        let chunks = chunk_js_file(src, JsDialect::JavaScript);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, "export_statement");
    }

    // ---- default export expression ----

    #[test]
    fn default_export_identifier() {
        let src = "function MyComponent() { return null; }\nexport default MyComponent;\n";
        let chunks = chunk_js_file(src, JsDialect::JavaScript);
        assert_eq!(chunks.len(), 2);
        // First: the function
        assert_eq!(chunks[0].chunk_type, "function_declaration");
        assert_eq!(chunks[0].entity_name.as_deref(), Some("MyComponent"));
        // Second: the default export
        assert_eq!(chunks[1].chunk_type, "export_statement");
        assert_eq!(chunks[1].entity_name.as_deref(), Some("MyComponent"));
    }

    // ---- mixed file ----

    #[test]
    fn mixed_js_file() {
        let src = r#"import React from 'react';

const API_URL = 'http://localhost:3000';

export function useUsers() {
  const [users, setUsers] = React.useState([]);
  return { users, setUsers };
}

export class UserService {
  getUser(id) { return fetch(`${API_URL}/users/${id}`); }
}

export default UserService;
"#;
        let chunks = chunk_js_file(src, JsDialect::JavaScript);
        // API_URL (lexical_declaration) + useUsers (function) + UserService (class) + default export
        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0].entity_name.as_deref(), Some("API_URL"));
        assert_eq!(chunks[1].entity_name.as_deref(), Some("useUsers"));
        assert_eq!(chunks[2].entity_name.as_deref(), Some("UserService"));
        assert_eq!(chunks[3].chunk_type, "export_statement");
    }

    // ---- dialect detection ----

    #[test]
    fn dialect_from_extension() {
        assert_eq!(
            JsDialect::from_extension(".js"),
            Some(JsDialect::JavaScript)
        );
        assert_eq!(JsDialect::from_extension(".jsx"), Some(JsDialect::Jsx));
        assert_eq!(
            JsDialect::from_extension(".ts"),
            Some(JsDialect::TypeScript)
        );
        assert_eq!(JsDialect::from_extension(".tsx"), Some(JsDialect::Tsx));
        assert_eq!(JsDialect::from_extension(".rs"), None);
    }

    #[test]
    fn language_id_mapping() {
        assert_eq!(JsDialect::JavaScript.language_id(), "javascript");
        assert_eq!(JsDialect::Jsx.language_id(), "javascript");
        assert_eq!(JsDialect::TypeScript.language_id(), "typescript");
        assert_eq!(JsDialect::Tsx.language_id(), "typescript");
    }

    // ---- hook detection via arrow function ----

    #[test]
    fn hook_as_arrow_function() {
        let src = r#"export const useCounter = () => {
  const [count, setCount] = useState(0);
  return { count, increment: () => setCount(c => c + 1) };
};
"#;
        let chunks = chunk_js_file(src, JsDialect::TypeScript);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, "arrow_function");
        assert_eq!(chunks[0].entity_name.as_deref(), Some("useCounter"));
    }

    // ---- GraphQL tagged template ----

    #[test]
    fn graphql_query_const() {
        let src = r#"export const GET_USER = gql`
  query GetUser($id: ID!) {
    user(id: $id) {
      id
      name
    }
  }
`;
"#;
        let chunks = chunk_js_file(src, JsDialect::TypeScript);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].entity_name.as_deref(), Some("GET_USER"));
    }

    // ---- TypeScript with complex types ----

    #[test]
    fn typescript_generic_function() {
        let src = r#"export function identity<T>(value: T): T {
  return value;
}
"#;
        let chunks = chunk_js_file(src, JsDialect::TypeScript);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_type, "function_declaration");
        assert_eq!(chunks[0].entity_name.as_deref(), Some("identity"));
    }
}
