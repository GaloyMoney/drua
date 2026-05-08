//! Line-grep helpers used by `apply_fetch_query`'s `Grep` mode.

/// Args bundle for [`filter_lines_rich`] — keeps the function signature
/// readable as the option set grew (asymmetric context, line numbers,
/// invert, head_limit).
pub(super) struct FilterArgs<'a, 'r> {
    pub lines: &'a [&'a str],
    pub re: &'r regex::Regex,
    pub invert: bool,
    /// Lines of context before each match (`rg -B`).
    pub before: usize,
    /// Lines of context after each match (`rg -A`).
    pub after: usize,
    /// Prefix kept lines with `<1-based-line-no>:` (`rg -n`).
    pub line_numbers: bool,
    /// Cap output to first N kept lines after context expansion.
    pub head_limit: Option<usize>,
}

/// `invert=true` keeps non-matching lines (rg's `-v`); context expansion
/// applies only when `invert=false`. `before`/`after` are independent and
/// asymmetric — `rg -A 3 -B 1` is `before=1, after=3`. Context-separator
/// `--` is emitted between non-contiguous match windows. Returned strings
/// are owned only when `line_numbers=true`; otherwise they're borrowed
/// from the input (zero-copy).
pub(super) fn filter_lines_rich(args: FilterArgs<'_, '_>) -> Vec<String> {
    let FilterArgs {
        lines,
        re,
        invert,
        before,
        after,
        line_numbers,
        head_limit,
    } = args;

    let with_no = |i: usize, s: &str| -> String {
        if line_numbers {
            format!("{}:{}", i + 1, s)
        } else {
            s.to_string()
        }
    };

    if invert || (before == 0 && after == 0) {
        let out: Vec<String> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| re.is_match(line) != invert)
            .map(|(i, line)| with_no(i, line))
            .collect();
        return apply_head_limit(out, head_limit);
    }

    let mut included = vec![false; lines.len()];
    for (i, line) in lines.iter().enumerate() {
        if re.is_match(line) {
            let start = i.saturating_sub(before);
            let end = (i + after + 1).min(lines.len());
            for flag in &mut included[start..end] {
                *flag = true;
            }
        }
    }

    let mut result: Vec<String> = Vec::new();
    let mut prev_included = false;
    for (i, line) in lines.iter().enumerate() {
        if included[i] {
            if !prev_included && !result.is_empty() {
                result.push("--".to_string());
            }
            result.push(with_no(i, line));
            prev_included = true;
        } else {
            prev_included = false;
        }
    }

    apply_head_limit(result, head_limit)
}

fn apply_head_limit(lines: Vec<String>, head_limit: Option<usize>) -> Vec<String> {
    match head_limit {
        Some(n) if lines.len() > n => lines.into_iter().take(n).collect(),
        _ => lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args<'a, 'r>(lines: &'a [&'a str], re: &'r regex::Regex) -> FilterArgs<'a, 'r> {
        FilterArgs {
            lines,
            re,
            invert: false,
            before: 0,
            after: 0,
            line_numbers: false,
            head_limit: None,
        }
    }

    #[test]
    fn basic_match_no_context() {
        let re = regex::Regex::new("err").unwrap();
        let lines = vec!["info ok", "error bad", "warn maybe"];
        let out = filter_lines_rich(args(&lines, &re));
        assert_eq!(out, vec!["error bad"]);
    }

    #[test]
    fn invert_drops_matches() {
        let re = regex::Regex::new("err").unwrap();
        let lines = vec!["info ok", "error bad", "warn maybe"];
        let out = filter_lines_rich(FilterArgs {
            invert: true,
            ..args(&lines, &re)
        });
        assert_eq!(out, vec!["info ok", "warn maybe"]);
    }

    #[test]
    fn symmetric_context_via_before_and_after_equal() {
        let re = regex::Regex::new("MATCH").unwrap();
        let lines = vec!["a", "b", "MATCH", "d", "e", "f", "g", "MATCH", "i"];
        let out = filter_lines_rich(FilterArgs {
            before: 1,
            after: 1,
            ..args(&lines, &re)
        });
        assert_eq!(out, vec!["b", "MATCH", "d", "--", "g", "MATCH", "i"]);
    }

    #[test]
    fn asymmetric_context_after_only() {
        let re = regex::Regex::new("MATCH").unwrap();
        let lines = vec!["a", "MATCH", "b", "c", "d"];
        let out = filter_lines_rich(FilterArgs {
            before: 0,
            after: 2,
            ..args(&lines, &re)
        });
        assert_eq!(out, vec!["MATCH", "b", "c"]);
    }

    #[test]
    fn asymmetric_context_before_only() {
        let re = regex::Regex::new("MATCH").unwrap();
        let lines = vec!["a", "b", "c", "MATCH", "d"];
        let out = filter_lines_rich(FilterArgs {
            before: 2,
            after: 0,
            ..args(&lines, &re)
        });
        assert_eq!(out, vec!["b", "c", "MATCH"]);
    }

    #[test]
    fn line_numbers_prefix_uses_1_based_index() {
        let re = regex::Regex::new("MATCH").unwrap();
        let lines = vec!["a", "MATCH", "b"];
        let out = filter_lines_rich(FilterArgs {
            line_numbers: true,
            ..args(&lines, &re)
        });
        assert_eq!(out, vec!["2:MATCH"]);
    }

    #[test]
    fn line_numbers_in_context_blocks_use_original_indices() {
        let re = regex::Regex::new("MATCH").unwrap();
        let lines = vec!["a", "b", "MATCH", "d", "e"];
        let out = filter_lines_rich(FilterArgs {
            before: 1,
            after: 1,
            line_numbers: true,
            ..args(&lines, &re)
        });
        assert_eq!(out, vec!["2:b", "3:MATCH", "4:d"]);
    }

    #[test]
    fn head_limit_truncates_kept_lines() {
        let re = regex::Regex::new("err").unwrap();
        let lines = vec!["err 1", "err 2", "err 3", "err 4"];
        let out = filter_lines_rich(FilterArgs {
            head_limit: Some(2),
            ..args(&lines, &re)
        });
        assert_eq!(out, vec!["err 1", "err 2"]);
    }

    #[test]
    fn head_limit_after_context_expansion() {
        let re = regex::Regex::new("MATCH").unwrap();
        let lines = vec!["a", "MATCH", "b", "c", "MATCH", "d"];
        // before=1 + after=1 with a separator would give 8 lines; head_limit
        // truncates to first 3.
        let out = filter_lines_rich(FilterArgs {
            before: 1,
            after: 1,
            head_limit: Some(3),
            ..args(&lines, &re)
        });
        assert_eq!(out, vec!["a", "MATCH", "b"]);
    }

    #[test]
    fn overlapping_context_no_separator() {
        let re = regex::Regex::new("M").unwrap();
        let lines = vec!["a", "M", "b", "M", "c"];
        let out = filter_lines_rich(FilterArgs {
            before: 1,
            after: 1,
            ..args(&lines, &re)
        });
        assert_eq!(out, vec!["a", "M", "b", "M", "c"]);
    }
}
