//! Filesystem facade over the server-side library clone, scoped to
//! `space:<slug>/...` paths. Used by the top-level file tools (Read,
//! LS, Glob, Grep, Edit, Move, Delete) to dispatch space-rooted ops
//! without needing an attached sandbox.
//!
//! All reads go through the bare clone via libgit2 — no on-disk
//! materialisation, no `tokio::fs`, no `rg` subprocess. The path
//! model is purely relative (`<rel_path>` inside `spaces/<slug>/`);
//! callers never see absolute filesystem paths.
//!
//! Every public method takes a raw `path: &str` and returns
//! `Result<Option<T>, ProjectError>`. `Ok(None)` signals "not a `space:`
//! path — fall through to the existing sandbox dispatch"; `Ok(Some(_))`
//! is a successful space-rooted op. Authorization runs once in
//! `resolve` via `Projects::space_for_subject`, which gates project
//! agents on the mount and lets admins reach any space; per-method
//! docs only call out method-specific behaviour.

use std::sync::Arc;

use serde_json::Value;
use tracing::instrument;

use drua_library::{Space, SpaceError, Spaces};

use crate::audit::Audit;
use crate::auth::AuthSubject;
use crate::project::{ProjectError, Projects};
use crate::user::Users;

/// Soft cap on `view_file` to keep tool responses bounded. Larger than
/// the typical sandbox `text_editor view` ceiling; agents needing more
/// should `grep` first.
const MAX_VIEW_FILE_BYTES: usize = 1_048_576; // 1 MiB

/// Result of `view_file` — files return their content; directory paths
/// return their listing so callers can format consistently.
pub enum FileView {
    File(String),
    Dir(Vec<String>),
}

/// Parsed view of a `space:<slug>` or `space:<slug>/<rel>` path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SpaceRef<'a> {
    slug: &'a str,
    /// Empty for the space root (`space:<slug>` or `space:<slug>/`).
    rel_path: &'a str,
}

/// Returns `Some(SpaceRef)` iff `path` starts with the `space:` prefix
/// and has a non-empty slug. Anything else returns `None` so callers
/// can fall through to the existing sandbox dispatch.
fn parse_space_path(path: &str) -> Option<SpaceRef<'_>> {
    let rest = path.strip_prefix("space:")?;
    let (slug, rel) = match rest.split_once('/') {
        Some((slug, rel)) => (slug, rel),
        None => (rest, ""),
    };
    if slug.is_empty() {
        return None;
    }
    Some(SpaceRef {
        slug,
        rel_path: rel,
    })
}

/// Auth-gated, resolved view of a `space:<slug>/<rel>` path.
struct Resolved {
    space: Space,
    /// Owned so the bundle outlives the input `&str`.
    rel_path: String,
}

#[derive(Clone)]
pub struct SpaceFs {
    spaces: Arc<Spaces>,
    projects: Arc<Projects>,
    users: Arc<Users>,
}

impl SpaceFs {
    pub fn new(spaces: Arc<Spaces>, projects: Arc<Projects>, users: Arc<Users>) -> Self {
        Self {
            spaces,
            projects,
            users,
        }
    }

    /// Pure peek — does `path` start with the `space:` prefix and have
    /// a non-empty slug? Useful for short-circuiting tool dispatch
    /// before any auth or IO.
    pub fn is_space_path(path: &str) -> bool {
        let Some(rest) = path.strip_prefix("space:") else {
            return false;
        };
        let slug = rest.split_once('/').map_or(rest, |(s, _)| s);
        !slug.is_empty()
    }

    /// Parses `path`, runs the auth gate, normalises and validates
    /// the rel-path, and bundles the resolved `(Space, rel_path)`.
    /// `Ok(None)` means `path` isn't a space path.
    async fn resolve(
        &self,
        sub: &AuthSubject,
        path: &str,
    ) -> Result<Option<Resolved>, ProjectError> {
        let Some(sref) = parse_space_path(path) else {
            return Ok(None);
        };
        let space = self.projects.space_for_subject(sub, sref.slug).await?;
        let rel_path = normalize_rel_path(sref.rel_path);
        Self::validate_rel_path(&rel_path)?;
        Ok(Some(Resolved { space, rel_path }))
    }

    /// View a file (or list a directory) under a `space:` path. Reads
    /// go through `Spaces::read_file` / `Spaces::list_dir` — straight
    /// from the bare clone via libgit2, no on-disk materialisation.
    #[instrument(name = "library.space_fs.view_file", skip(self, sub))]
    pub async fn view_file(
        &self,
        sub: &AuthSubject,
        path: &str,
        view_range: Option<(i64, i64)>,
    ) -> Result<Option<FileView>, ProjectError> {
        let Some(resolved) = self.resolve(sub, path).await? else {
            return Ok(None);
        };

        // Try as a directory first; if it's a tree, list it. If not a
        // tree, fall through to a blob read.
        if let Some(entries) = self
            .spaces
            .list_dir(&resolved.space.slug, &resolved.rel_path)
            .await
            .map_err(|e| -> ProjectError { e.into() })?
        {
            return Ok(Some(FileView::Dir(format_dir(entries))));
        }

        let bytes = self
            .spaces
            .read_file(&resolved.space.slug, &resolved.rel_path)
            .await
            .map_err(|e| -> ProjectError { e.into() })?
            .ok_or_else(|| io_err(format!("no such file: {}", resolved.rel_path)))?;
        if bytes.len() > MAX_VIEW_FILE_BYTES {
            return Err(io_err(format!(
                "file too large ({} bytes > {} cap); use `grep` to extract a slice",
                bytes.len(),
                MAX_VIEW_FILE_BYTES
            ))
            .into());
        }
        let content = String::from_utf8(bytes).map_err(|e| io_err(format!("non-utf8: {e}")))?;
        Ok(Some(FileView::File(apply_view_range(&content, view_range))))
    }

    /// List a directory under a `space:` path.
    #[instrument(name = "library.space_fs.view_dir", skip(self, sub))]
    pub async fn view_dir(
        &self,
        sub: &AuthSubject,
        path: &str,
    ) -> Result<Option<Vec<String>>, ProjectError> {
        let Some(resolved) = self.resolve(sub, path).await? else {
            return Ok(None);
        };
        let entries = self
            .spaces
            .list_dir(&resolved.space.slug, &resolved.rel_path)
            .await
            .map_err(|e| -> ProjectError { e.into() })?
            .ok_or_else(|| io_err(format!("no such directory: {}", resolved.rel_path)))?;
        Ok(Some(format_dir(entries)))
    }

    /// Blind overwrite of `space:<slug>/<rel>` with `content`.
    #[instrument(name = "library.space_fs.write_file", skip(self, sub, content))]
    pub async fn write_file(
        &self,
        sub: &AuthSubject,
        path: &str,
        content: String,
    ) -> Result<Option<()>, ProjectError> {
        let Some(resolved) = self.resolve(sub, path).await? else {
            return Ok(None);
        };
        Audit::record_action_if_unset("space.write_file");
        let attribution = Audit::commit_attribution(&self.users).await;
        self.spaces
            .write_file(
                &resolved.space.slug,
                &resolved.rel_path,
                content,
                attribution,
            )
            .await
            .map_err(|e| -> ProjectError { e.into() })?;
        Ok(Some(()))
    }

    /// `text_editor` `str_replace`. The unique-occurrence check happens
    /// at the worker (`Spaces::str_replace`) against the freshest disk
    /// state.
    #[instrument(
        name = "library.space_fs.str_replace",
        skip(self, sub, old_str, new_str)
    )]
    pub async fn str_replace(
        &self,
        sub: &AuthSubject,
        path: &str,
        old_str: String,
        new_str: String,
    ) -> Result<Option<()>, ProjectError> {
        let Some(resolved) = self.resolve(sub, path).await? else {
            return Ok(None);
        };
        Audit::record_action_if_unset("space.str_replace");
        let attribution = Audit::commit_attribution(&self.users).await;
        self.spaces
            .str_replace(
                &resolved.space.slug,
                &resolved.rel_path,
                old_str,
                new_str,
                attribution,
            )
            .await
            .map_err(|e| -> ProjectError { e.into() })?;
        Ok(Some(()))
    }

    /// `text_editor` `insert`. Insertion happens at the worker against
    /// the freshest disk state.
    #[instrument(name = "library.space_fs.insert_line", skip(self, sub, text))]
    pub async fn insert_line(
        &self,
        sub: &AuthSubject,
        path: &str,
        line_number: usize,
        text: String,
    ) -> Result<Option<()>, ProjectError> {
        let Some(resolved) = self.resolve(sub, path).await? else {
            return Ok(None);
        };
        Audit::record_action_if_unset("space.insert");
        let attribution = Audit::commit_attribution(&self.users).await;
        self.spaces
            .insert(
                &resolved.space.slug,
                &resolved.rel_path,
                line_number,
                text,
                attribution,
            )
            .await
            .map_err(|e| -> ProjectError { e.into() })?;
        Ok(Some(()))
    }

    /// Removes the file at `space:<slug>/<rel>`. Success even if the
    /// file was already gone.
    #[instrument(name = "library.space_fs.delete_file", skip(self, sub))]
    pub async fn delete_file(
        &self,
        sub: &AuthSubject,
        path: &str,
    ) -> Result<Option<()>, ProjectError> {
        let Some(resolved) = self.resolve(sub, path).await? else {
            return Ok(None);
        };
        Audit::record_action_if_unset("space.delete_file");
        let attribution = Audit::commit_attribution(&self.users).await;
        self.spaces
            .delete_file(&resolved.space.slug, &resolved.rel_path, attribution)
            .await
            .map_err(|e| -> ProjectError { e.into() })?;
        Ok(Some(()))
    }

    /// Renames `from` → `to` within a single space. `Ok(None)` only when
    /// _both_ paths are sandbox paths (caller falls through). Mixed
    /// space/sandbox or cross-space moves are hard errors so the agent
    /// gets a clear message instead of silent fall-through.
    #[instrument(name = "library.space_fs.move_file", skip(self, sub))]
    pub async fn move_file(
        &self,
        sub: &AuthSubject,
        from: &str,
        to: &str,
    ) -> Result<Option<()>, ProjectError> {
        let from_is_space = Self::is_space_path(from);
        let to_is_space = Self::is_space_path(to);
        if !from_is_space && !to_is_space {
            return Ok(None);
        }
        if from_is_space != to_is_space {
            return Err(SpaceError::CrossSpaceMove {
                from_slug: parse_space_path(from)
                    .map(|r| r.slug.to_string())
                    .unwrap_or_else(|| "<sandbox>".to_string()),
                to_slug: parse_space_path(to)
                    .map(|r| r.slug.to_string())
                    .unwrap_or_else(|| "<sandbox>".to_string()),
            }
            .into());
        }
        let Some(from_resolved) = self.resolve(sub, from).await? else {
            return Ok(None);
        };
        let Some(to_ref) = parse_space_path(to) else {
            return Ok(None);
        };
        if from_resolved.space.slug != to_ref.slug {
            return Err(SpaceError::CrossSpaceMove {
                from_slug: from_resolved.space.slug.clone(),
                to_slug: to_ref.slug.to_string(),
            }
            .into());
        }
        let to_rel = normalize_rel_path(to_ref.rel_path);
        Self::validate_rel_path(&to_rel)?;
        Audit::record_action_if_unset("space.move_file");
        let attribution = Audit::commit_attribution(&self.users).await;
        self.spaces
            .move_file(
                &from_resolved.space.slug,
                &from_resolved.rel_path,
                &to_rel,
                attribution,
            )
            .await
            .map_err(|e| -> ProjectError { e.into() })?;
        Ok(Some(()))
    }

    /// Glob walk across the space's tree. Pattern is the standard
    /// glob syntax (`*`, `**`, `?`); matches against the relative
    /// path inside `spaces/<slug>/`. `path`'s rel-component anchors
    /// the search root.
    #[instrument(name = "library.space_fs.glob", skip(self, sub))]
    pub async fn glob(
        &self,
        sub: &AuthSubject,
        path: &str,
        pattern: &str,
    ) -> Result<Option<Vec<String>>, ProjectError> {
        let Some(resolved) = self.resolve(sub, path).await? else {
            return Ok(None);
        };
        let blobs = self
            .spaces
            .walk(&resolved.space.slug, &resolved.rel_path)
            .await
            .map_err(|e| -> ProjectError { e.into() })?;
        Ok(Some(glob_blobs(blobs, pattern)?))
    }

    /// Grep walk across the space's tree. Replicates the curated subset
    /// of flags the `Grep` top-level tool exposes, but without `rg` —
    /// runs each blob's content through the `regex` crate.
    #[instrument(name = "library.space_fs.grep", skip(self, sub, args))]
    pub async fn grep(
        &self,
        sub: &AuthSubject,
        path: &str,
        args: &Value,
    ) -> Result<Option<String>, ProjectError> {
        let Some(resolved) = self.resolve(sub, path).await? else {
            return Ok(None);
        };
        let blobs = self
            .spaces
            .walk(&resolved.space.slug, &resolved.rel_path)
            .await
            .map_err(|e| -> ProjectError { e.into() })?;
        Ok(Some(grep_blobs(blobs, args)?))
    }

    /// Rejects path-traversal, absolute paths, NUL bytes, and leading `/`.
    /// Empty `rel` is allowed (means "the space root").
    pub(crate) fn validate_rel_path(rel: &str) -> Result<(), SpaceError> {
        if rel.is_empty() {
            return Ok(());
        }
        if rel.contains('\0') || rel.starts_with('/') || rel.starts_with('\\') {
            return Err(invalid_rel_path(rel));
        }
        for segment in rel.split('/') {
            if segment.is_empty() || segment == "." || segment == ".." {
                return Err(invalid_rel_path(rel));
            }
        }
        Ok(())
    }
}

/// Strips `.` segments — the no-op CWD shorthand. Callers (`resolve`,
/// `move_file`) normalise before validation so agents can pass `.` /
/// `./foo` / `foo/./bar` interchangeably with `""` / `foo` / `foo/bar`.
/// Does NOT collapse `..` (real traversal); those still fail validation.
fn normalize_rel_path(rel: &str) -> String {
    rel.split('/')
        .filter(|seg| *seg != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// Filter `blobs` (rel-path, bytes) by a glob pattern and return the
/// matching paths, sorted.
fn glob_blobs(blobs: Vec<(String, Vec<u8>)>, pattern: &str) -> Result<Vec<String>, SpaceError> {
    let regex = glob_to_regex(pattern)
        .map_err(|e| io_err(format!("invalid glob pattern '{pattern}': {e}")))?;
    let mut out: Vec<String> = blobs
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| regex.is_match(p))
        .collect();
    out.sort();
    Ok(out)
}

/// Run `grep` over already-walked blobs. Mirrors the curated subset of
/// flags the `Grep` top-level tool accepts.
fn grep_blobs(blobs: Vec<(String, Vec<u8>)>, args: &Value) -> Result<String, SpaceError> {
    let pattern = args
        .get("pattern")
        .and_then(|v| v.as_str())
        .ok_or_else(|| io_err("grep: missing 'pattern'".to_string()))?;
    let mode = args
        .get("output_mode")
        .and_then(|v| v.as_str())
        .unwrap_or("files_with_matches");
    let case_insensitive = args.get("-i").and_then(|v| v.as_bool()).unwrap_or(false);
    let multiline = args
        .get("multiline")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let glob_filter = args
        .get("glob")
        .and_then(|v| v.as_str())
        .map(glob_to_regex)
        .transpose()
        .map_err(|e| io_err(format!("invalid glob filter: {e}")))?;
    let show_line_nums = args.get("-n").and_then(|v| v.as_bool()).unwrap_or(true);
    let context_after = args.get("-A").and_then(|v| v.as_i64()).unwrap_or(0).max(0) as usize;
    let context_before = args.get("-B").and_then(|v| v.as_i64()).unwrap_or(0).max(0) as usize;
    let context_around = args.get("-C").and_then(|v| v.as_i64()).unwrap_or(0).max(0) as usize;
    let head_limit = args
        .get("head_limit")
        .and_then(|v| v.as_i64())
        .map(|n| n.max(0) as usize);

    let regex = regex::RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .multi_line(multiline)
        .dot_matches_new_line(multiline)
        .build()
        .map_err(|e| io_err(format!("invalid regex '{pattern}': {e}")))?;

    let before = context_before.max(context_around);
    let after = context_after.max(context_around);

    let mut output_lines: Vec<String> = Vec::new();
    for (rel, bytes) in blobs {
        if let Some(g) = glob_filter.as_ref() {
            if !g.is_match(&rel) {
                continue;
            }
        }
        let Ok(content) = std::str::from_utf8(&bytes) else {
            continue;
        };

        match mode {
            "files_with_matches" => {
                if regex.is_match(content) {
                    output_lines.push(rel);
                }
            }
            "count" => {
                let n = regex.find_iter(content).count();
                if n > 0 {
                    output_lines.push(format!("{rel}:{n}"));
                }
            }
            _ => {
                let lines: Vec<&str> = content.lines().collect();
                let mut matched_idx: Vec<usize> = Vec::new();
                for (i, line) in lines.iter().enumerate() {
                    if regex.is_match(line) {
                        matched_idx.push(i);
                    }
                }
                if matched_idx.is_empty() {
                    continue;
                }
                // Build the context-window line set (deduped).
                let mut want: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
                for i in &matched_idx {
                    let lo = i.saturating_sub(before);
                    let hi = (*i + after).min(lines.len().saturating_sub(1));
                    for j in lo..=hi {
                        want.insert(j);
                    }
                }
                for idx in want {
                    if show_line_nums {
                        output_lines.push(format!("{rel}:{}:{}", idx + 1, lines[idx]));
                    } else {
                        output_lines.push(format!("{rel}:{}", lines[idx]));
                    }
                }
            }
        }
    }

    if let Some(cap) = head_limit {
        output_lines.truncate(cap);
    }
    Ok(output_lines.join("\n"))
}

fn invalid_rel_path(rel: &str) -> SpaceError {
    SpaceError::InvalidRelPath {
        path: rel.to_string(),
        reason: "must be relative; no '..', leading '/', empty segments, or NUL bytes".into(),
    }
}

fn format_dir(entries: Vec<drua_library::DirEntry>) -> Vec<String> {
    entries
        .into_iter()
        .map(|e| {
            if e.is_dir {
                format!("{}/", e.name)
            } else {
                e.name
            }
        })
        .collect()
}

fn io_err(msg: String) -> SpaceError {
    SpaceError::Io(msg)
}

/// Translate a glob pattern (`*`, `**`, `?`) into a regex anchored at
/// both ends. Single `*` matches a single path segment (no `/`); `**`
/// matches zero or more segments.
fn glob_to_regex(pattern: &str) -> Result<regex::Regex, regex::Error> {
    let mut out = String::from("^");
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' => {
                if i + 1 < chars.len() && chars[i + 1] == '*' {
                    out.push_str(".*");
                    i += 2;
                    // `**/` collapses the trailing slash too so the
                    // pattern matches at any directory depth.
                    if i < chars.len() && chars[i] == '/' {
                        i += 1;
                    }
                } else {
                    out.push_str("[^/]*");
                    i += 1;
                }
            }
            '?' => {
                out.push_str("[^/]");
                i += 1;
            }
            '.' | '+' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '\\' => {
                out.push('\\');
                out.push(c);
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out.push('$');
    regex::Regex::new(&out)
}

/// Slice `content` to the requested 1-based, inclusive line range.
/// `end == -1` (or unset) means EOF.
fn apply_view_range(content: &str, view_range: Option<(i64, i64)>) -> String {
    let Some((start, end)) = view_range else {
        return content.to_string();
    };
    let start = start.max(1) as usize;
    let lines: Vec<&str> = content.lines().collect();
    let end_idx = if end < 0 || end as usize > lines.len() {
        lines.len()
    } else {
        end as usize
    };
    if start > end_idx {
        return String::new();
    }
    lines[start - 1..end_idx].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_strips_prefix_and_slug() {
        let r = parse_space_path("space:oncall/runbooks/foo.md").unwrap();
        assert_eq!(r.slug, "oncall");
        assert_eq!(r.rel_path, "runbooks/foo.md");
    }

    #[test]
    fn parse_root_with_trailing_slash() {
        let r = parse_space_path("space:oncall/").unwrap();
        assert_eq!(r.slug, "oncall");
        assert_eq!(r.rel_path, "");
    }

    #[test]
    fn parse_root_without_slash() {
        let r = parse_space_path("space:oncall").unwrap();
        assert_eq!(r.slug, "oncall");
        assert_eq!(r.rel_path, "");
    }

    #[test]
    fn parse_returns_none_for_non_space_paths() {
        assert!(parse_space_path("/etc/passwd").is_none());
        assert!(parse_space_path("oncall/foo.md").is_none());
        assert!(parse_space_path("").is_none());
    }

    #[test]
    fn parse_rejects_empty_slug() {
        assert!(parse_space_path("space:").is_none());
        assert!(parse_space_path("space:/foo").is_none());
    }

    #[test]
    fn validate_rejects_traversal() {
        assert!(SpaceFs::validate_rel_path("../etc").is_err());
        assert!(SpaceFs::validate_rel_path("foo/../etc").is_err());
    }

    #[test]
    fn normalize_strips_dot_segments() {
        assert_eq!(normalize_rel_path("."), "");
        assert_eq!(normalize_rel_path("./"), "");
        assert_eq!(normalize_rel_path("./foo"), "foo");
        assert_eq!(normalize_rel_path("foo/."), "foo");
        assert_eq!(normalize_rel_path("foo/./bar"), "foo/bar");
        assert_eq!(normalize_rel_path("foo/bar"), "foo/bar");
        assert_eq!(normalize_rel_path(""), "");
    }

    #[test]
    fn normalize_then_validate_accepts_dot_paths() {
        for raw in [".", "./", "./foo", "foo/.", "foo/./bar"] {
            let normalized = normalize_rel_path(raw);
            assert!(
                SpaceFs::validate_rel_path(&normalized).is_ok(),
                "expected `{raw}` (normalised: `{normalized}`) to validate"
            );
        }
    }

    #[test]
    fn normalize_does_not_collapse_traversal() {
        assert_eq!(normalize_rel_path(".."), "..");
        assert_eq!(normalize_rel_path("foo/../bar"), "foo/../bar");
        assert!(SpaceFs::validate_rel_path(&normalize_rel_path("..")).is_err());
        assert!(SpaceFs::validate_rel_path(&normalize_rel_path("foo/../bar")).is_err());
    }

    #[test]
    fn validate_rejects_absolute() {
        assert!(SpaceFs::validate_rel_path("/etc/passwd").is_err());
        assert!(SpaceFs::validate_rel_path("\\windows").is_err());
    }

    #[test]
    fn validate_rejects_nul() {
        assert!(SpaceFs::validate_rel_path("foo\0bar").is_err());
    }

    #[test]
    fn validate_rejects_double_slash() {
        assert!(SpaceFs::validate_rel_path("foo//bar").is_err());
    }

    #[test]
    fn validate_accepts_normal_paths() {
        assert!(SpaceFs::validate_rel_path("README.md").is_ok());
        assert!(SpaceFs::validate_rel_path("runbooks/foo/bar.md").is_ok());
        assert!(SpaceFs::validate_rel_path("").is_ok());
    }

    #[test]
    fn view_range_inclusive() {
        let txt = "a\nb\nc\nd\ne";
        assert_eq!(apply_view_range(txt, Some((2, 4))), "b\nc\nd");
    }

    #[test]
    fn view_range_open_end() {
        let txt = "a\nb\nc";
        assert_eq!(apply_view_range(txt, Some((2, -1))), "b\nc");
    }

    #[test]
    fn view_range_clamps_overshoot() {
        let txt = "a\nb";
        assert_eq!(apply_view_range(txt, Some((1, 10))), "a\nb");
    }

    #[test]
    fn view_range_empty_when_inverted() {
        let txt = "a\nb\nc";
        assert_eq!(apply_view_range(txt, Some((3, 1))), "");
    }

    #[test]
    fn view_range_none_returns_full() {
        let txt = "a\nb\nc";
        assert_eq!(apply_view_range(txt, None), "a\nb\nc");
    }

    #[test]
    fn glob_star_matches_one_segment() {
        let r = glob_to_regex("*.md").unwrap();
        assert!(r.is_match("foo.md"));
        assert!(!r.is_match("foo/bar.md"));
    }

    #[test]
    fn glob_double_star_matches_any_depth() {
        let r = glob_to_regex("**/*.md").unwrap();
        assert!(r.is_match("foo.md"));
        assert!(r.is_match("foo/bar.md"));
        assert!(r.is_match("a/b/c/x.md"));
    }

    #[test]
    fn glob_question_matches_one_char() {
        let r = glob_to_regex("?.md").unwrap();
        assert!(r.is_match("a.md"));
        assert!(!r.is_match("ab.md"));
    }
}
