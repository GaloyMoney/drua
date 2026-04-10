use rmcp::model::{CallToolResult, Content};
use serde::Deserialize;

use super::ToolSetsError;

const MAX_PATTERN_LENGTH: usize = 1000;
const DEFAULT_TAIL: usize = 1000;

/// Post-processing filter applied to tool output to reduce token usage.
///
/// Processing order: grep -> head -> tail.
/// When no filter is provided by the caller, [`OutputFilter::global_default`]
/// is used to cap output at [`DEFAULT_TAIL`] lines.
#[derive(Debug, Deserialize, Default)]
pub struct OutputFilter {
    /// Regex pattern to filter output lines (only matching lines returned).
    pub grep: Option<String>,
    /// Exclude matching lines instead of including them (grep -v).
    #[serde(default)]
    pub invert_match: Option<bool>,
    /// Lines of context around grep matches (grep -C).
    pub context_lines: Option<usize>,
    /// Return only the first N lines.
    pub head: Option<usize>,
    /// Return only the last N lines.
    pub tail: Option<usize>,
}

impl OutputFilter {
    /// A sensible default filter applied when the caller does not provide one.
    /// Caps output at [`DEFAULT_TAIL`] lines to prevent token blowups.
    pub fn global_default() -> Self {
        Self {
            tail: Some(DEFAULT_TAIL),
            ..Default::default()
        }
    }

    /// Apply the filter to a [`CallToolResult`], returning a new result with
    /// filtered text content. Non-text content blocks are dropped.
    pub fn apply(&self, result: CallToolResult) -> Result<CallToolResult, ToolSetsError> {
        if self.grep.is_none() && self.head.is_none() && self.tail.is_none() {
            return Ok(result);
        }

        let text = extract_text(&result);
        let lines: Vec<&str> = text.lines().collect();

        // 1. grep
        let filtered = if let Some(pattern) = &self.grep {
            if pattern.len() > MAX_PATTERN_LENGTH {
                return Err(ToolSetsError::InvalidArgument(format!(
                    "grep pattern too long ({} chars, max {MAX_PATTERN_LENGTH})",
                    pattern.len()
                )));
            }
            let re = regex::Regex::new(pattern)
                .map_err(|e| ToolSetsError::InvalidArgument(format!("invalid grep regex: {e}")))?;
            let invert = self.invert_match.unwrap_or(false);
            filter_lines(&lines, &re, invert, self.context_lines)
        } else {
            lines
        };

        // 2. head
        let filtered = if let Some(n) = self.head {
            filtered.into_iter().take(n).collect()
        } else {
            filtered
        };

        // 3. tail
        let filtered: Vec<&str> = if let Some(n) = self.tail {
            let len = filtered.len();
            if len > n {
                filtered.into_iter().skip(len - n).collect()
            } else {
                filtered
            }
        } else {
            filtered
        };

        let output = filtered.join("\n");
        let mut filtered_result = CallToolResult::success(vec![Content::text(output)]);
        filtered_result.is_error = result.is_error;
        Ok(filtered_result)
    }
}

/// Extract all text content from a [`CallToolResult`] into a single string.
fn extract_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            rmcp::model::RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Filter lines by regex pattern with optional context lines.
///
/// When `invert` is false, returns lines that match the pattern (grep).
/// When `invert` is true, returns lines that do NOT match (grep -v).
/// When `context` is Some(n), includes n lines before/after each match (grep -C).
pub(crate) fn filter_lines<'a>(
    lines: &[&'a str],
    re: &regex::Regex,
    invert: bool,
    context: Option<usize>,
) -> Vec<&'a str> {
    if context.is_none() || invert {
        return lines
            .iter()
            .filter(|line| re.is_match(line) != invert)
            .copied()
            .collect();
    }

    let ctx = context.unwrap_or(0);
    let mut included = vec![false; lines.len()];

    for (i, line) in lines.iter().enumerate() {
        if re.is_match(line) {
            let start = i.saturating_sub(ctx);
            let end = (i + ctx + 1).min(lines.len());
            for flag in &mut included[start..end] {
                *flag = true;
            }
        }
    }

    let mut result = Vec::new();
    let mut prev_included = false;
    for (i, line) in lines.iter().enumerate() {
        if included[i] {
            if !prev_included && !result.is_empty() {
                result.push("--");
            }
            result.push(line);
            prev_included = true;
        } else {
            prev_included = false;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_result(text: &str) -> CallToolResult {
        CallToolResult::success(vec![Content::text(text)])
    }

    #[test]
    fn no_filter_passes_through() {
        let filter = OutputFilter::default();
        let result = text_result("line1\nline2\nline3");
        let filtered = filter.apply(result).unwrap();
        let text = extract_text(&filtered);
        assert_eq!(text, "line1\nline2\nline3");
    }

    #[test]
    fn grep_only() {
        let filter = OutputFilter {
            grep: Some("error".to_string()),
            ..Default::default()
        };
        let result = text_result("info: ok\nerror: bad\ninfo: fine\nerror: worse");
        let filtered = filter.apply(result).unwrap();
        let text = extract_text(&filtered);
        assert_eq!(text, "error: bad\nerror: worse");
    }

    #[test]
    fn grep_invert() {
        let filter = OutputFilter {
            grep: Some("error".to_string()),
            invert_match: Some(true),
            ..Default::default()
        };
        let result = text_result("info: ok\nerror: bad\ninfo: fine");
        let filtered = filter.apply(result).unwrap();
        let text = extract_text(&filtered);
        assert_eq!(text, "info: ok\ninfo: fine");
    }

    #[test]
    fn grep_with_context() {
        let filter = OutputFilter {
            grep: Some("MATCH".to_string()),
            context_lines: Some(1),
            ..Default::default()
        };
        let result = text_result("a\nb\nMATCH\nd\ne\nf\ng\nMATCH\ni");
        let filtered = filter.apply(result).unwrap();
        let text = extract_text(&filtered);
        assert_eq!(text, "b\nMATCH\nd\n--\ng\nMATCH\ni");
    }

    #[test]
    fn head_only() {
        let filter = OutputFilter {
            head: Some(2),
            ..Default::default()
        };
        let result = text_result("a\nb\nc\nd");
        let filtered = filter.apply(result).unwrap();
        let text = extract_text(&filtered);
        assert_eq!(text, "a\nb");
    }

    #[test]
    fn tail_only() {
        let filter = OutputFilter {
            tail: Some(2),
            ..Default::default()
        };
        let result = text_result("a\nb\nc\nd");
        let filtered = filter.apply(result).unwrap();
        let text = extract_text(&filtered);
        assert_eq!(text, "c\nd");
    }

    #[test]
    fn grep_then_tail() {
        let filter = OutputFilter {
            grep: Some("line".to_string()),
            tail: Some(2),
            ..Default::default()
        };
        let result = text_result("line 1\nnope\nline 2\nline 3");
        let filtered = filter.apply(result).unwrap();
        let text = extract_text(&filtered);
        assert_eq!(text, "line 2\nline 3");
    }

    #[test]
    fn head_then_tail() {
        let filter = OutputFilter {
            head: Some(3),
            tail: Some(2),
            ..Default::default()
        };
        let result = text_result("a\nb\nc\nd\ne");
        let filtered = filter.apply(result).unwrap();
        let text = extract_text(&filtered);
        // head(3) → a,b,c then tail(2) → b,c
        assert_eq!(text, "b\nc");
    }

    #[test]
    fn invalid_regex_returns_error() {
        let filter = OutputFilter {
            grep: Some("[invalid".to_string()),
            ..Default::default()
        };
        let result = text_result("test");
        let err = filter.apply(result).unwrap_err();
        assert!(err.to_string().contains("invalid grep regex"));
    }

    #[test]
    fn pattern_too_long_returns_error() {
        let filter = OutputFilter {
            grep: Some("x".repeat(1001)),
            ..Default::default()
        };
        let result = text_result("test");
        let err = filter.apply(result).unwrap_err();
        assert!(err.to_string().contains("grep pattern too long"));
    }

    #[test]
    fn empty_content_passes_through() {
        let filter = OutputFilter {
            grep: Some("test".to_string()),
            ..Default::default()
        };
        let result = CallToolResult::success(vec![]);
        let filtered = filter.apply(result).unwrap();
        let text = extract_text(&filtered);
        assert_eq!(text, "");
    }

    #[test]
    fn preserves_is_error_flag() {
        let filter = OutputFilter {
            head: Some(1),
            ..Default::default()
        };
        let mut result = text_result("a\nb");
        result.is_error = Some(true);
        let filtered = filter.apply(result).unwrap();
        assert_eq!(filtered.is_error, Some(true));
    }

    // ── filter_lines unit tests ─────────────────────────────────────

    #[test]
    fn filter_lines_basic_match() {
        let re = regex::Regex::new("err").unwrap();
        let lines = vec!["info ok", "error bad", "warn maybe"];
        let out = filter_lines(&lines, &re, false, None);
        assert_eq!(out, vec!["error bad"]);
    }

    #[test]
    fn filter_lines_invert() {
        let re = regex::Regex::new("err").unwrap();
        let lines = vec!["info ok", "error bad", "warn maybe"];
        let out = filter_lines(&lines, &re, true, None);
        assert_eq!(out, vec!["info ok", "warn maybe"]);
    }

    #[test]
    fn filter_lines_context_separator() {
        let re = regex::Regex::new("MATCH").unwrap();
        let lines = vec!["a", "b", "MATCH", "d", "e", "f", "g", "MATCH", "i"];
        let out = filter_lines(&lines, &re, false, Some(1));
        assert_eq!(out, vec!["b", "MATCH", "d", "--", "g", "MATCH", "i"]);
    }

    #[test]
    fn global_default_caps_at_default_tail() {
        let filter = OutputFilter::global_default();
        let many_lines: String = (0..1500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = text_result(&many_lines);
        let filtered = filter.apply(result).unwrap();
        let text = extract_text(&filtered);
        assert_eq!(text.lines().count(), DEFAULT_TAIL);
        // Should keep the last DEFAULT_TAIL lines
        assert!(text.starts_with(&format!("line {}", 1500 - DEFAULT_TAIL)));
    }

    #[test]
    fn filter_lines_overlapping_context() {
        let re = regex::Regex::new("M").unwrap();
        let lines = vec!["a", "M", "b", "M", "c"];
        let out = filter_lines(&lines, &re, false, Some(1));
        // contexts overlap → no separator
        assert_eq!(out, vec!["a", "M", "b", "M", "c"]);
    }
}
