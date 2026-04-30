//! Line-by-line V4A patch parser → `Vec<EditOp>`.
//!
//! Translation rules:
//! - `*** Add File: <path>` + `+`-prefixed lines → [`EditOp::Create`].
//! - `*** Delete File: <path>` → [`EditOp::Delete`].
//! - `*** Update File: <path>` with `@@` hunks → one [`EditOp::StrReplace`]
//!   per hunk; `old_str` = context + `-` lines, `new_str` = context + `+` lines.
//! - `*** Move to: <new-path>` after Update File → emits a trailing
//!   [`EditOp::Rename`] (after the StrReplace ops on the source path).

use super::super::super::canonical::{EditOp, EditParseError};

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";

pub fn parse_v4a_patch(raw: &str) -> Result<Vec<EditOp>, EditParseError> {
    let normalised = raw.replace("\r\n", "\n");
    let trimmed = normalised.trim_matches(['\n', ' ', '\t']);
    let lines: Vec<&str> = trimmed.split('\n').collect();

    if lines.is_empty() || lines[0].trim() != BEGIN {
        return Err(EditParseError::MalformedPatch(
            "missing `*** Begin Patch` header".into(),
        ));
    }
    if lines
        .iter()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim())
        != Some(END)
    {
        return Err(EditParseError::MalformedPatch(
            "missing `*** End Patch` footer".into(),
        ));
    }

    let mut ops = Vec::new();
    let mut i = 1usize;

    while i < lines.len() {
        let line = lines[i];
        let trimmed_line = line.trim_end();

        if trimmed_line.is_empty() {
            i += 1;
            continue;
        }
        if trimmed_line == END {
            break;
        }

        if let Some(path) = trimmed_line.strip_prefix("*** Add File: ") {
            i += 1;
            let (content, next) = collect_added_lines(&lines, i)?;
            ops.push(EditOp::Create {
                path: path.trim().to_string(),
                file_text: content,
            });
            i = next;
        } else if let Some(path) = trimmed_line.strip_prefix("*** Delete File: ") {
            ops.push(EditOp::Delete {
                path: path.trim().to_string(),
            });
            i += 1;
        } else if let Some(path) = trimmed_line.strip_prefix("*** Update File: ") {
            let path = path.trim().to_string();
            i += 1;

            let mut move_to: Option<String> = None;
            if i < lines.len() {
                if let Some(target) = lines[i].trim_end().strip_prefix("*** Move to: ") {
                    move_to = Some(target.trim().to_string());
                    i += 1;
                }
            }

            let (replacements, next) = collect_update_hunks(&lines, i, &path)?;
            ops.extend(replacements);
            i = next;

            if let Some(to) = move_to {
                ops.push(EditOp::Rename { from: path, to });
            }
        } else {
            return Err(EditParseError::MalformedPatch(format!(
                "unexpected line outside file section: {trimmed_line:?}"
            )));
        }
    }

    if ops.is_empty() {
        return Err(EditParseError::MalformedPatch(
            "patch contained no file operations".into(),
        ));
    }

    Ok(ops)
}

/// Collect `+`-prefixed body lines of an `*** Add File:` section. Stops at the
/// next `*** ` directive or `*** End Patch`.
fn collect_added_lines(lines: &[&str], start: usize) -> Result<(String, usize), EditParseError> {
    let mut out = Vec::new();
    let mut i = start;
    while i < lines.len() {
        let line = lines[i];
        if line.trim_end().starts_with("*** ") {
            break;
        }
        if let Some(rest) = line.strip_prefix('+') {
            out.push(rest.to_string());
        } else if line.is_empty() {
            out.push(String::new());
        } else {
            return Err(EditParseError::MalformedPatch(format!(
                "Add File body line must start with `+`: {line:?}"
            )));
        }
        i += 1;
    }
    Ok((out.join("\n"), i))
}

/// Collect `@@`-delimited hunks for an `*** Update File:` section. Each hunk
/// becomes one `StrReplace`. Stops at the next `*** ` directive.
fn collect_update_hunks(
    lines: &[&str],
    start: usize,
    path: &str,
) -> Result<(Vec<EditOp>, usize), EditParseError> {
    let mut ops = Vec::new();
    let mut i = start;
    let mut current_old: Vec<String> = Vec::new();
    let mut current_new: Vec<String> = Vec::new();
    let mut in_hunk = false;

    let flush = |old: &mut Vec<String>, new: &mut Vec<String>, ops: &mut Vec<EditOp>| {
        if !old.is_empty() || !new.is_empty() {
            ops.push(EditOp::StrReplace {
                path: path.to_string(),
                old_str: old.join("\n"),
                new_str: new.join("\n"),
            });
        }
        old.clear();
        new.clear();
    };

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_end();

        if trimmed.starts_with("*** ") && trimmed != "*** End of File" {
            break;
        }

        if let Some(_header) = trimmed.strip_prefix("@@") {
            if in_hunk {
                flush(&mut current_old, &mut current_new, &mut ops);
            }
            in_hunk = true;
            i += 1;
            continue;
        }

        if !in_hunk {
            if trimmed.is_empty() {
                i += 1;
                continue;
            }
            return Err(EditParseError::MalformedPatch(format!(
                "Update File body must begin with `@@`: {trimmed:?}"
            )));
        }

        if trimmed == "*** End of File" {
            i += 1;
            continue;
        }

        if let Some(rest) = line.strip_prefix('+') {
            current_new.push(rest.to_string());
        } else if let Some(rest) = line.strip_prefix('-') {
            current_old.push(rest.to_string());
        } else if let Some(rest) = line.strip_prefix(' ') {
            current_old.push(rest.to_string());
            current_new.push(rest.to_string());
        } else if line.is_empty() {
            current_old.push(String::new());
            current_new.push(String::new());
        } else {
            return Err(EditParseError::MalformedPatch(format!(
                "hunk line must begin with ` `, `+`, or `-`: {line:?}"
            )));
        }
        i += 1;
    }

    if in_hunk {
        flush(&mut current_old, &mut current_new, &mut ops);
    }

    if ops.is_empty() {
        return Err(EditParseError::MalformedPatch(format!(
            "Update File `{path}` contained no hunks"
        )));
    }

    Ok((ops, i))
}
