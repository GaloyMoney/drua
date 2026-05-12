//! Concourse build-log preprocessor: strips ANSI escapes, the
//! `[HH:MM:SS] ` timestamp prefix that the upstream emits on every
//! line, and re-flows `\r`-overwriting progress (git clone / nix copy
//! / curl style) into one logical line per intermediate state.
//! Line-aligned with the input so downstream line-aware passes
//! (string-summarizer chain, byte/line elide) can do their work
//! against clean text.

use std::sync::OnceLock;

use regex::Regex;

/// Tool-name suffixes this preprocessor matches. Production concourse
/// tools use `concourse_get_build_logs`; the bats fake-upstream fixture
/// uses `concourse-build-log` (catalog naming convention).
pub const TOOL_NAMES: &[&str] = &["concourse_get_build_logs", "concourse-build-log"];

/// Strip ANSI colour codes, leading `[HH:MM:SS] ` timestamps, and
/// translate every `\r` into `\n` so downstream line-mode elision (and
/// the existing line-pattern chain passes — `<git-clone>`, `<nix-copy>`,
/// etc.) see one progress increment per line instead of a single
/// multi-KB `\r`-packed mega-line. Idempotent.
pub fn run(raw: &str) -> String {
    let timestamp_re = timestamp_re();
    let reflowed = reflow_carriage_returns(raw);
    let mut out = String::with_capacity(reflowed.len());
    for line in reflowed.lines() {
        let no_ansi = strip_ansi(line);
        let body = strip_timestamp(&no_ansi, timestamp_re);
        out.push_str(body);
        out.push('\n');
    }
    out
}

/// Translate `\r` runs into `\n`s. `git clone`, `nix copy`, `curl`, …
/// use bare `\r` to rewrite the same terminal line in-place; what
/// reaches a captured log is one `\n`-terminated line of
/// `seg1\rseg2\r…\rfinal` that can run to tens of KB. Splitting each
/// such mega-line back into one logical line per intermediate state
/// lets the existing string-summarizer chain match the progress
/// patterns it already knows about (e.g. `<git-clone>` collapses runs
/// of `remote: …` lines). `\r\n` is normalised to `\n` first so we
/// don't double-newline Windows-style endings.
fn reflow_carriage_returns(raw: &str) -> std::borrow::Cow<'_, str> {
    if !raw.contains('\r') {
        return std::borrow::Cow::Borrowed(raw);
    }
    let mut out = String::with_capacity(raw.len());
    let normalised = raw.replace("\r\n", "\n");
    for ch in normalised.chars() {
        if ch == '\r' {
            out.push('\n');
        } else {
            out.push(ch);
        }
    }
    std::borrow::Cow::Owned(out)
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

    #[test]
    fn reflows_carriage_return_progress_into_separate_lines() {
        let raw = "[08:53:32] remote: Counting objects:   1% (21/2027)\rremote: Counting objects:  50% (1014/2027)\rremote: Counting objects: 100% (2027/2027), done.\n[08:53:33] next line\n";
        let cleaned = run(raw);
        assert_eq!(
            cleaned,
            "remote: Counting objects:   1% (21/2027)\n\
             remote: Counting objects:  50% (1014/2027)\n\
             remote: Counting objects: 100% (2027/2027), done.\n\
             next line\n"
        );
    }

    #[test]
    fn normalises_crlf_without_doubling_newlines() {
        let raw = "[12:00:00] a\r\n[12:00:01] b\r\n";
        let cleaned = run(raw);
        assert_eq!(cleaned, "a\nb\n");
    }
}
