//! Concourse build-log preprocessor: strips ANSI escapes, the
//! `[HH:MM:SS] ` timestamp prefix that the upstream emits on every
//! line, and re-flows `\r`-overwriting progress (git clone / nix copy
//! / curl style) into one logical line per intermediate state.
//!
//! Owns its tool's canonical shape: production concourse emits
//! `{logs: <raw text>}` on the structured channel. `preprocess()`
//! consumes that root, replaces the `logs` value with the cleaned
//! text, and returns the new root plus `$.logs` as the path where the
//! text now lives — the walker uses that as the `<summary path>`
//! attribute and as the location for line-mode elision.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use super::PreprocessedRoot;

/// Tool-name suffixes this preprocessor matches. Production concourse
/// tools use `concourse_get_build_logs`; the bats fake-upstream fixture
/// uses `concourse-build-log` (catalog naming convention). Matched by
/// suffix so a catalog prefix (e.g. `fake_upstream_concourse-build-log`)
/// doesn't break detection.
const TOOL_NAMES: &[&str] = &["concourse_get_build_logs", "concourse-build-log"];

fn tool_name_matches(tool_name: &str) -> bool {
    TOOL_NAMES.iter().any(|n| tool_name.ends_with(n))
}

/// Look for `{logs: <string>}` at the root and run the preprocessor on
/// the `logs` value when found. Returns the rebuilt root with the
/// transformed `logs`, the json-path `$.logs`, and the line mapping
/// back to raw bytes.
pub(super) fn preprocess(tool_name: &str, root: &Value) -> Option<PreprocessedRoot> {
    if !tool_name_matches(tool_name) {
        return None;
    }
    let map = root.as_object()?;
    let logs = map.get("logs")?.as_str()?;
    let (cleaned, preprocessed_to_raw) = transform(logs);
    let mut new_map = map.clone();
    new_map.insert("logs".to_string(), Value::String(cleaned));
    Some(PreprocessedRoot {
        root: Value::Object(new_map),
        root_path: "$.logs".to_string(),
        preprocessed_to_raw,
        prefer_tail: true,
    })
}

/// Strip ANSI, `[HH:MM:SS] ` timestamps, split each `\r`-packed raw
/// line into one output line per intermediate state. Returns the
/// cleaned text plus a per-output-line index back to the raw line.
/// Idempotent.
fn transform(raw: &str) -> (String, Vec<u32>) {
    let timestamp_re = timestamp_re();
    let mut out = String::with_capacity(raw.len());
    let mut preprocessed_to_raw = Vec::new();
    for (raw_idx, raw_line) in raw.lines().enumerate() {
        // `str::lines` trims a trailing `\r`, so only intermediate
        // `\r`s appear as in-line content here.
        for segment in raw_line.split('\r') {
            let no_ansi = strip_ansi(segment);
            let body = strip_timestamp(&no_ansi, timestamp_re);
            out.push_str(body);
            out.push('\n');
            preprocessed_to_raw.push(raw_idx as u32);
        }
    }
    (out, preprocessed_to_raw)
}

fn strip_ansi(s: &str) -> std::borrow::Cow<'_, str> {
    static ANSI: OnceLock<Regex> = OnceLock::new();
    let re = ANSI.get_or_init(|| Regex::new(r"\x1b\[[0-9;]*m").expect("ansi regex"));
    re.replace_all(s, "")
}

fn strip_timestamp<'a>(line: &'a str, re: &Regex) -> &'a str {
    if let Some(m) = re.find(line) {
        &line[m.end()..]
    } else {
        line
    }
}

fn timestamp_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^\[\d{2}:\d{2}:\d{2}\] ?").expect("timestamp regex"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn transform_strips_ansi_and_timestamps() {
        let raw = "\x1b[32m[12:34:56] hello\x1b[0m\n[12:34:57] world\n";
        let (text, mapping) = transform(raw);
        assert_eq!(text, "hello\nworld\n");
        assert_eq!(mapping, vec![0, 1]);
    }

    #[test]
    fn transform_is_idempotent_on_clean_text() {
        let raw = "already clean\nno escapes\n";
        let (text, mapping) = transform(raw);
        assert_eq!(text, raw);
        assert_eq!(mapping, vec![0, 1]);
    }

    #[test]
    fn transform_reflows_carriage_return_progress() {
        let raw = "[08:53:32] remote: Counting objects:   1% (21/2027)\rremote: Counting objects:  50% (1014/2027)\rremote: Counting objects: 100% (2027/2027), done.\n[08:53:33] next line\n";
        let (text, mapping) = transform(raw);
        assert_eq!(
            text,
            "remote: Counting objects:   1% (21/2027)\n\
             remote: Counting objects:  50% (1014/2027)\n\
             remote: Counting objects: 100% (2027/2027), done.\n\
             next line\n"
        );
        assert_eq!(mapping, vec![0, 0, 0, 1]);
    }

    #[test]
    fn transform_normalises_crlf() {
        let raw = "[12:00:00] a\r\n[12:00:01] b\r\n";
        let (text, mapping) = transform(raw);
        assert_eq!(text, "a\nb\n");
        assert_eq!(mapping, vec![0, 1]);
    }

    #[test]
    fn preprocess_returns_none_for_other_tools() {
        let root = json!({"logs": "[10:00:00] hi"});
        assert!(preprocess("bash", &root).is_none());
    }

    #[test]
    fn preprocess_returns_none_when_root_has_no_logs_field() {
        let root = json!({"something_else": "x"});
        assert!(preprocess("concourse-build-log", &root).is_none());
    }

    #[test]
    fn preprocess_returns_transformed_root_for_concourse_shape() {
        let root = json!({"logs": "[10:00:00] hello\n[10:00:01] world\n", "extra": "kept"});
        let out = preprocess("concourse-build-log", &root).expect("matched");
        assert_eq!(out.root_path, "$.logs");
        assert_eq!(out.root, json!({"logs": "hello\nworld\n", "extra": "kept"}),);
        assert_eq!(out.preprocessed_to_raw, vec![0, 1]);
        assert!(out.prefer_tail, "CI logs should bias to tail");
    }
}
