//! Shared `cat -n`-style line-number formatter for `Read` output.
//! Same format Anthropic's `str_replace_based_edit_tool view` uses
//! (right-aligned 6-char line number + tab + content), so the agent
//! sees identical shape whether it reads a sandbox file or a
//! `space:<slug>/...` file.

/// `(start, end)` are 1-based inclusive; `end` clamps to the line count.
pub fn number_lines(content: &str, view_range: Option<(usize, usize)>) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    let (start, end) = match view_range {
        Some((s, e)) => (s.max(1), e.min(lines.len())),
        None => (1, lines.len()),
    };
    if start > end {
        return String::new();
    }
    lines[start - 1..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>6}\t{line}", start + i))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cat_n_style_six_char_right_aligned_tab() {
        let out = number_lines("alpha\nbravo\ncharlie\n", None);
        assert_eq!(out, "     1\talpha\n     2\tbravo\n     3\tcharlie");
    }

    #[test]
    fn view_range_starts_numbering_from_start() {
        let out = number_lines("a\nb\nc\nd\ne\n", Some((3, 5)));
        assert_eq!(out, "     3\tc\n     4\td\n     5\te");
    }

    #[test]
    fn end_clamps_to_line_count() {
        let out = number_lines("a\nb\nc\n", Some((1, 999)));
        assert_eq!(out, "     1\ta\n     2\tb\n     3\tc");
    }

    #[test]
    fn empty_content_yields_empty_output() {
        assert_eq!(number_lines("", None), "");
    }

    #[test]
    fn line_numbers_grow_to_six_chars() {
        let mut content = String::new();
        for i in 1..=1000 {
            content.push_str(&format!("line{i}\n"));
        }
        let out = number_lines(&content, Some((1000, 1000)));
        assert_eq!(out, "  1000\tline1000");
    }
}
