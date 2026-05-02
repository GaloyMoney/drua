//! Filesystem facade over the server-side library clone, scoped to
//! `space:<slug>/...` paths. Used by the top-level file tools (Read,
//! LS, Glob, Grep, Edit, Move, Delete) to dispatch space-rooted ops
//! without needing an attached sandbox.
//!
//! Every public method takes a raw `path: &str` and returns
//! `Result<Option<T>, ProjectError>`. `Ok(None)` signals "not a `space:`
//! path — fall through to the existing sandbox dispatch"; `Ok(Some(_))`
//! is a successful space-rooted op. Authorization runs once in
//! `resolve` via `Projects::space_for_subject`; per-method docs only
//! call out method-specific behaviour.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;
use tokio::process::Command;
use tracing::instrument;

use crate::auth::AuthSubject;
use crate::library::Space;
use crate::project::{ProjectError, Projects};

use super::space_path;
use super::Library;

/// Soft cap on `view_file` to keep tool responses bounded. Larger than
/// the typical sandbox `text_editor view` ceiling; agents needing more
/// should `grep` first.
const MAX_VIEW_FILE_BYTES: u64 = 1_048_576; // 1 MiB

/// Result of `view_file` — files return their content; directory paths
/// return their listing so callers can format consistently.
pub enum FileView {
    File(String),
    Dir(Vec<String>),
}

/// Auth-gated, resolved view of a `space:<slug>/<rel>` path.
struct Resolved {
    space: Space,
    /// Owned so the bundle outlives the input `&str`.
    rel_path: String,
    /// Absolute on-disk path (`<library-clone>/spaces/<slug>/<rel>`).
    full_path: PathBuf,
}

#[derive(Clone)]
pub struct SpaceFs {
    library: Arc<Library>,
    projects: Arc<Projects>,
}

impl SpaceFs {
    pub fn new(library: Arc<Library>, projects: Arc<Projects>) -> Self {
        Self { library, projects }
    }

    /// Pure peek — does `path` start with the `space:` prefix and have
    /// a non-empty slug? Useful for short-circuiting tool dispatch
    /// before any auth or IO.
    pub fn is_space_path(path: &str) -> bool {
        space_path::parse(path).is_some()
    }

    /// Parses `path`, runs the auth gate, validates the rel-path,
    /// and bundles the resolved `(Space, rel_path, full_path)`.
    /// `Ok(None)` means `path` isn't a space path.
    async fn resolve(
        &self,
        sub: &AuthSubject,
        path: &str,
    ) -> Result<Option<Resolved>, ProjectError> {
        let Some(sref) = space_path::parse(path) else {
            return Ok(None);
        };
        let space = self.projects.space_for_subject(sub, sref.slug).await?;
        space_path::validate_rel_path(sref.rel_path)?;
        let mut full = self.library.space_root(&space.slug);
        if !sref.rel_path.is_empty() {
            full.push(sref.rel_path);
        }
        Ok(Some(Resolved {
            space,
            rel_path: sref.rel_path.to_string(),
            full_path: full,
        }))
    }

    /// View a file (or list a directory) under a `space:` path.
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
        let full = &resolved.full_path;
        let metadata = tokio::fs::metadata(full)
            .await
            .map_err(|e| io_err(format!("stat {}: {e}", full.display())))?;
        if metadata.is_dir() {
            return Ok(Some(FileView::Dir(read_dir_names(full).await?)));
        }
        if metadata.len() > MAX_VIEW_FILE_BYTES {
            return Err(io_err(format!(
                "file too large ({} bytes > {} cap); use `grep` to extract a slice",
                metadata.len(),
                MAX_VIEW_FILE_BYTES
            ))
            .into());
        }
        let content = tokio::fs::read_to_string(full)
            .await
            .map_err(|e| io_err(format!("read {}: {e}", full.display())))?;
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
        Ok(Some(read_dir_names(&resolved.full_path).await?))
    }

    /// Blind overwrite of `space:<slug>/<rel>` with `content`.
    /// Used for the `text_editor` `create` command.
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
        self.library
            .write_space_file(&resolved.space, resolved.rel_path, content)
            .await?;
        Ok(Some(()))
    }

    /// `text_editor` `str_replace`. The unique-occurrence check
    /// happens at the worker against the freshest disk state — see
    /// `WriteToRuntimeRunner::str_replace_space_file_op`.
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
        self.library
            .str_replace_space_file(&resolved.space, resolved.rel_path, old_str, new_str)
            .await?;
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
        self.library
            .insert_space_file(&resolved.space, resolved.rel_path, line_number, text)
            .await?;
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
        self.library
            .delete_space_file(&resolved.space, resolved.rel_path)
            .await?;
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
            return Err(super::SpaceError::CrossSpaceMove {
                from_slug: space_path::parse(from)
                    .map(|r| r.slug.to_string())
                    .unwrap_or_else(|| "<sandbox>".to_string()),
                to_slug: space_path::parse(to)
                    .map(|r| r.slug.to_string())
                    .unwrap_or_else(|| "<sandbox>".to_string()),
            }
            .into());
        }
        let Some(from_resolved) = self.resolve(sub, from).await? else {
            return Ok(None);
        };
        let Some(to_ref) = space_path::parse(to) else {
            return Ok(None);
        };
        if from_resolved.space.slug != to_ref.slug {
            return Err(super::SpaceError::CrossSpaceMove {
                from_slug: from_resolved.space.slug.clone(),
                to_slug: to_ref.slug.to_string(),
            }
            .into());
        }
        space_path::validate_rel_path(to_ref.rel_path)?;
        self.library
            .move_space_file(
                &from_resolved.space,
                from_resolved.rel_path,
                to_ref.rel_path.to_string(),
            )
            .await?;
        Ok(Some(()))
    }

    /// Glob via `rg --files -g <pattern>`, anchored at the space root.
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
        let full = &resolved.full_path;
        let mut cmd = Command::new("rg");
        cmd.arg("--files").arg("-g").arg(pattern).arg(full);
        let out = cmd
            .output()
            .await
            .map_err(|e| io_err(format!("spawn rg: {e}")))?;
        // rg exits 1 when no matches; treat that as empty output, not error.
        if !out.status.success() && out.status.code() != Some(1) {
            return Err(io_err(format!(
                "rg --files failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ))
            .into());
        }
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        // Strip the absolute prefix so paths look like `runbooks/foo.md`,
        // not `/var/.../spaces/<slug>/runbooks/foo.md`.
        let prefix = format!("{}/", full.display());
        let lines: Vec<String> = stdout
            .lines()
            .map(|l| l.strip_prefix(&prefix).unwrap_or(l).to_string())
            .collect();
        Ok(Some(lines))
    }

    /// Grep via `rg`, anchored at the space root. Forwards a curated
    /// subset of flags that the `Grep` top-level tool exposes.
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
        let full = &resolved.full_path;
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or_else(|| io_err("grep: missing 'pattern'".to_string()))?;

        let mut cmd = Command::new("rg");
        cmd.arg("--no-heading");

        // Output mode mirrors the sandbox-server's Grep handler.
        let mode = args
            .get("output_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("files_with_matches");
        match mode {
            "content" => {
                let show_line_nums = args.get("-n").and_then(|v| v.as_bool()).unwrap_or(true);
                if show_line_nums {
                    cmd.arg("-n");
                }
            }
            "count" => {
                cmd.arg("-c");
            }
            _ => {
                cmd.arg("-l");
            }
        }
        if args.get("-i").and_then(|v| v.as_bool()).unwrap_or(false) {
            cmd.arg("-i");
        }
        if let Some(n) = args.get("-A").and_then(|v| v.as_i64()) {
            cmd.arg(format!("-A{n}"));
        }
        if let Some(n) = args.get("-B").and_then(|v| v.as_i64()) {
            cmd.arg(format!("-B{n}"));
        }
        if let Some(n) = args.get("-C").and_then(|v| v.as_i64()) {
            cmd.arg(format!("-C{n}"));
        }
        if args
            .get("multiline")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            cmd.arg("-U").arg("--multiline-dotall");
        }
        if let Some(g) = args.get("glob").and_then(|v| v.as_str()) {
            cmd.arg("-g").arg(g);
        }
        if let Some(t) = args.get("type").and_then(|v| v.as_str()) {
            cmd.arg("--type").arg(t);
        }

        cmd.arg("--").arg(pattern).arg(full);

        let out = cmd
            .output()
            .await
            .map_err(|e| io_err(format!("spawn rg: {e}")))?;
        if !out.status.success() && out.status.code() != Some(1) {
            return Err(io_err(format!(
                "rg failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ))
            .into());
        }
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let prefix = format!("{}/", full.display());
        let rel: String = stdout
            .lines()
            .map(|l| l.strip_prefix(&prefix).unwrap_or(l))
            .collect::<Vec<_>>()
            .join("\n");

        if let Some(cap) = args.get("head_limit").and_then(|v| v.as_i64()) {
            let cap = cap.max(0) as usize;
            let head: Vec<&str> = rel.lines().take(cap).collect();
            return Ok(Some(head.join("\n")));
        }
        Ok(Some(rel))
    }
}

async fn read_dir_names(full: &std::path::Path) -> Result<Vec<String>, ProjectError> {
    let mut entries = tokio::fs::read_dir(full)
        .await
        .map_err(|e| io_err(format!("read_dir {}: {e}", full.display())))?;
    let mut names: Vec<String> = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| io_err(format!("read_dir {}: {e}", full.display())))?
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = entry
            .file_type()
            .await
            .map(|ft| if ft.is_dir() { "/" } else { "" })
            .unwrap_or("");
        names.push(format!("{name}{kind}"));
    }
    names.sort();
    Ok(names)
}

fn io_err(msg: String) -> super::SpaceError {
    super::SpaceError::Io(msg)
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
    use super::apply_view_range;

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
}
