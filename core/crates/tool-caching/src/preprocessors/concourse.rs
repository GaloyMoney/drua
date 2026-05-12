//! Concourse build-log preprocessor: strips ANSI escapes and the
//! `[HH:MM:SS] ` timestamp prefix that the upstream emits on every
//! line. Line-aligned with the input so downstream line-aware passes
//! (string-summarizer chain, byte/line elide) can do their work
//! against clean text.

use std::sync::OnceLock;

use regex::Regex;

/// Tool-name suffixes this preprocessor matches. Production concourse
/// tools use `concourse_get_build_logs`; the bats fake-upstream fixture
/// uses `concourse-build-log` (catalog naming convention).
pub const TOOL_NAMES: &[&str] = &["concourse_get_build_logs", "concourse-build-log"];

/// Strip ANSI colour codes and leading `[HH:MM:SS] ` timestamps from
/// every line in `raw`. Idempotent.
pub fn run(raw: &str) -> String {
    let timestamp_re = timestamp_re();
    let mut out = String::with_capacity(raw.len());
    for line in raw.lines() {
        let no_ansi = strip_ansi(line);
        let body = strip_timestamp(&no_ansi, timestamp_re);
        out.push_str(body);
        out.push('\n');
    }
    out
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
        let cleaned = run(raw);
        assert_eq!(cleaned, "hello\nworld\n");
    }

    #[test]
    fn idempotent_on_clean_text() {
        let raw = "already clean\nno escapes\n";
        assert_eq!(run(raw), raw);
    }
}
