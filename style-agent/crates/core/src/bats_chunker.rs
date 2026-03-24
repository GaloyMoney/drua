use crate::chunker::Chunk;

/// Chunk a bats/bash file into logical blocks using brace-depth tracking.
///
/// Extracts: `@test` blocks, named functions, and setup/teardown lifecycle hooks.
/// Each chunk is prefixed with the file header (variables, `load` statements, etc.).
pub fn chunk_bats_file(source: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = source.lines().collect();
    let header = extract_file_header(&lines);
    let mut chunks = Vec::new();

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if let Some(test_name) = parse_test_start(trimmed) {
            if let Some(chunk) = extract_block(&lines, i, "test_block", &test_name, &header) {
                i = chunk.line_end;
                chunks.push(chunk);
                continue;
            }
        } else if let Some((fn_name, kind)) = parse_function_start(trimmed) {
            if let Some(chunk) = extract_block(&lines, i, kind, &fn_name, &header) {
                i = chunk.line_end;
                chunks.push(chunk);
                continue;
            }
        }

        i += 1;
    }

    chunks
}

/// Collect the file header: shebang, comments, `load` statements, and top-level
/// variable assignments that appear before the first function or @test block.
fn extract_file_header(lines: &[&str]) -> String {
    let mut header_lines = Vec::new();

    for line in lines {
        let trimmed = line.trim();

        // Stop at the first function / test definition
        if parse_test_start(trimmed).is_some() || parse_function_start(trimmed).is_some() {
            break;
        }

        // Keep shebang, comments, load, and variable assignments
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with("load ")
            || trimmed.starts_with("load \"")
            || trimmed.contains('=')
        {
            header_lines.push(*line);
        }
    }

    // Trim trailing blank lines
    while header_lines.last().is_some_and(|l| l.trim().is_empty()) {
        header_lines.pop();
    }

    if header_lines.is_empty() {
        String::new()
    } else {
        header_lines.join("\n")
    }
}

/// Detect `@test "some name" {` and return the test name.
fn parse_test_start(line: &str) -> Option<String> {
    if !line.starts_with("@test ") {
        return None;
    }
    // Extract name between quotes
    let after = &line["@test ".len()..];
    let start = after.find('"')? + 1;
    let end = after[start..].find('"')? + start;
    Some(after[start..end].to_string())
}

/// Detect function definitions. Returns `(name, chunk_type)`.
///
/// Recognised forms:
/// - `name() {`
/// - `function name {`
/// - `function name() {`
fn parse_function_start(line: &str) -> Option<(String, &'static str)> {
    // `function name ...`
    if let Some(rest) = line.strip_prefix("function ") {
        let name = rest
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .next()?;
        if name.is_empty() {
            return None;
        }
        let kind = lifecycle_kind(name);
        return Some((name.to_string(), kind));
    }

    // `name() {`
    if let Some(paren_pos) = line.find("()") {
        let candidate = line[..paren_pos].trim();
        if !candidate.is_empty() && candidate.chars().all(|c| c.is_alphanumeric() || c == '_') {
            let kind = lifecycle_kind(candidate);
            return Some((candidate.to_string(), kind));
        }
    }

    None
}

fn lifecycle_kind(name: &str) -> &'static str {
    match name {
        "setup" | "setup_file" => "setup_function",
        "teardown" | "teardown_file" => "teardown_function",
        _ => "helper_function",
    }
}

/// Starting at `start_line`, track brace depth to extract a complete block.
/// Returns a `Chunk` whose `line_start`/`line_end` are 1-based.
fn extract_block(
    lines: &[&str],
    start_line: usize,
    chunk_type: &str,
    entity_name: &str,
    header: &str,
) -> Option<Chunk> {
    let mut depth: i32 = 0;
    let mut found_open = false;
    let mut block_lines: Vec<&str> = Vec::new();

    for (offset, line) in lines[start_line..].iter().enumerate() {
        block_lines.push(line);

        for ch in line.chars() {
            if ch == '{' {
                depth += 1;
                found_open = true;
            } else if ch == '}' {
                depth -= 1;
            }
        }

        if found_open && depth == 0 {
            let body = block_lines.join("\n");
            let content = if header.is_empty() {
                body
            } else {
                format!("{header}\n\n{body}")
            };

            return Some(Chunk {
                content,
                chunk_type: chunk_type.to_string(),
                entity_name: Some(entity_name.to_string()),
                impl_type: None,
                impl_trait: None,
                line_start: start_line + 1,
                line_end: start_line + offset + 1,
                doc_comment: None,
            });
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_BATS: &str = r#"#!/usr/bin/env bats

# Integration tests for my-service

ENDPOINT="http://localhost:8080"
TEST_DB="test_db"

load "helpers"

setup_file() {
    start_services
}

teardown_file() {
    stop_services
}

_helper_request() {
    local path="$1"
    curl -sf "$ENDPOINT/$path"
}

@test "health check returns 200" {
    result=$(_helper_request "health")
    [ "$result" = "ok" ]
}

@test "create user works" {
    result=$(curl -sf -X POST "$ENDPOINT/users" \
        -d '{"name": "alice"}')
    echo "$result" | grep -q "alice"
}
"#;

    #[test]
    fn extracts_all_chunk_types() {
        let chunks = chunk_bats_file(SAMPLE_BATS);

        let types: Vec<&str> = chunks.iter().map(|c| c.chunk_type.as_str()).collect();
        assert_eq!(
            types,
            vec![
                "setup_function",
                "teardown_function",
                "helper_function",
                "test_block",
                "test_block",
            ]
        );
    }

    #[test]
    fn test_block_names() {
        let chunks = chunk_bats_file(SAMPLE_BATS);
        let test_names: Vec<&str> = chunks
            .iter()
            .filter(|c| c.chunk_type == "test_block")
            .filter_map(|c| c.entity_name.as_deref())
            .collect();
        assert_eq!(
            test_names,
            vec!["health check returns 200", "create user works"]
        );
    }

    #[test]
    fn helper_function_name() {
        let chunks = chunk_bats_file(SAMPLE_BATS);
        let helper = chunks
            .iter()
            .find(|c| c.chunk_type == "helper_function")
            .expect("should have a helper_function chunk");
        assert_eq!(helper.entity_name.as_deref(), Some("_helper_request"));
    }

    #[test]
    fn chunks_include_file_header() {
        let chunks = chunk_bats_file(SAMPLE_BATS);
        for chunk in &chunks {
            assert!(
                chunk.content.contains("ENDPOINT="),
                "chunk '{}' should contain header variable",
                chunk.entity_name.as_deref().unwrap_or("?")
            );
            assert!(
                chunk.content.contains("load \"helpers\""),
                "chunk '{}' should contain load statement",
                chunk.entity_name.as_deref().unwrap_or("?")
            );
        }
    }

    #[test]
    fn line_numbers_are_correct() {
        let chunks = chunk_bats_file(SAMPLE_BATS);
        let setup = chunks
            .iter()
            .find(|c| c.chunk_type == "setup_function")
            .unwrap();
        // setup_file() starts at line 10 (1-based)
        assert_eq!(setup.line_start, 10);
        assert_eq!(setup.line_end, 12);
    }

    #[test]
    fn empty_file_returns_no_chunks() {
        assert!(chunk_bats_file("").is_empty());
        assert!(chunk_bats_file("#!/usr/bin/env bats\n# just a comment\n").is_empty());
    }

    #[test]
    fn function_keyword_syntax() {
        let src = "function my_func {\n  echo hello\n}\n";
        let chunks = chunk_bats_file(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].entity_name.as_deref(), Some("my_func"));
        assert_eq!(chunks[0].chunk_type, "helper_function");
    }
}
