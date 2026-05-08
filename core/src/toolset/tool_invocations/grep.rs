//! Line-grep helper used by `apply_fetch_query`'s `Grep` mode.

/// `invert=false` keeps matches (grep); `invert=true` drops them (grep -v).
/// `context=Some(n)` includes n lines before/after each match (grep -C).
pub(super) fn filter_lines<'a>(
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
    fn filter_lines_overlapping_context() {
        let re = regex::Regex::new("M").unwrap();
        let lines = vec!["a", "M", "b", "M", "c"];
        let out = filter_lines(&lines, &re, false, Some(1));
        assert_eq!(out, vec!["a", "M", "b", "M", "c"]);
    }
}
