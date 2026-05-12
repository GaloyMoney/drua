//! Concourse build-log preprocessor: strips ANSI escapes, the
//! `[HH:MM:SS] ` timestamp prefix that the upstream emits on every
//! line, and re-flows `\r`-overwriting progress (git clone / nix copy
//! / curl style) into one logical line per intermediate state.
//! Line-aligned with the input so downstream line-aware passes
//! (string-summarizer chain, byte/line elide) can do their work
//! against clean text — and emits a `preprocessed_to_raw` mapping
//! tracking which raw line each output line came from (`\r`-reflow
//! emits multiple output lines per raw line, so the mapping is no
//! longer identity).

use std::sync::OnceLock;

use regex::Regex;

use super::Preprocessed;

/// Tool-name suffixes this preprocessor matches. Production concourse
/// tools use `concourse_get_build_logs`; the bats fake-upstream fixture
/// uses `concourse-build-log` (catalog naming convention).
pub const TOOL_NAMES: &[&str] = &["concourse_get_build_logs", "concourse-build-log"];

/// Strip ANSI colour codes, leading `[HH:MM:SS] ` timestamps, and split
/// each `\r`-packed raw line into one output line per intermediate
/// state. Returns the cleaned text plus the per-output-line index back
/// to the raw line each output line came from. Idempotent.
pub fn run(raw: &str) -> Preprocessed {
    let timestamp_re = timestamp_re();
    let mut out = String::with_capacity(raw.len());
    let mut preprocessed_to_raw = Vec::new();
    for (raw_idx, raw_line) in raw.lines().enumerate() {
        // `str::lines` already trims a trailing `\r` from the line,
        // so we only see intermediate `\r`s as content here.
        for segment in raw_line.split('\r') {
            let no_ansi = strip_ansi(segment);
            let body = strip_timestamp(&no_ansi, timestamp_re);
            out.push_str(body);
            out.push('\n');
            preprocessed_to_raw.push(raw_idx as u32);
        }
    }
    Preprocessed {
        text: out,
        preprocessed_to_raw,
    }
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

    #[test]
    fn strips_ansi_and_timestamps() {
        let raw = "\x1b[32m[12:34:56] hello\x1b[0m\n[12:34:57] world\n";
        let out = run(raw);
        assert_eq!(out.text, "hello\nworld\n");
        assert_eq!(out.preprocessed_to_raw, vec![0, 1]);
    }

    #[test]
    fn idempotent_on_clean_text() {
        let raw = "already clean\nno escapes\n";
        let out = run(raw);
        assert_eq!(out.text, raw);
        assert_eq!(out.preprocessed_to_raw, vec![0, 1]);
    }

    #[test]
    fn reflows_carriage_return_progress_into_separate_lines() {
        let raw = "[08:53:32] remote: Counting objects:   1% (21/2027)\rremote: Counting objects:  50% (1014/2027)\rremote: Counting objects: 100% (2027/2027), done.\n[08:53:33] next line\n";
        let out = run(raw);
        assert_eq!(
            out.text,
            "remote: Counting objects:   1% (21/2027)\n\
             remote: Counting objects:  50% (1014/2027)\n\
             remote: Counting objects: 100% (2027/2027), done.\n\
             next line\n"
        );
        // First three output lines come from raw line 0 (the `\r`-packed
        // progress); fourth output line comes from raw line 1.
        assert_eq!(out.preprocessed_to_raw, vec![0, 0, 0, 1]);
    }

    #[test]
    fn normalises_crlf_without_doubling_newlines() {
        let raw = "[12:00:00] a\r\n[12:00:01] b\r\n";
        let out = run(raw);
        assert_eq!(out.text, "a\nb\n");
        assert_eq!(out.preprocessed_to_raw, vec![0, 1]);
    }
}
