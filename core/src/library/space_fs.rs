//! Read-only filesystem facade over the server-side library clone.
//! Used by the top-level read tools when the path is `space:<slug>/...`.
//! Authorization gate: every op runs `Projects::space_for_subject` first.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;
use tokio::process::Command;
use tracing::instrument;

use crate::auth::AuthSubject;
use crate::project::{ProjectError, Projects};

use super::space_path::{self, SpaceRef};
use super::Library;

/// Soft cap on `view_file` to keep tool responses bounded. Larger than
/// the typical sandbox `text_editor view` ceiling; agents needing more
/// should `grep` first.
const MAX_VIEW_FILE_BYTES: u64 = 1_048_576; // 1 MiB

#[derive(Clone)]
pub struct SpaceFs {
    library: Arc<Library>,
    projects: Arc<Projects>,
}

impl SpaceFs {
    pub fn new(library: Arc<Library>, projects: Arc<Projects>) -> Self {
        Self { library, projects }
    }

    /// Resolves `<space-root>/<rel>` after running the auth gate.
    async fn resolve(
        &self,
        sub: &AuthSubject,
        sref: SpaceRef<'_>,
    ) -> Result<PathBuf, ProjectError> {
        let space = self.projects.space_for_subject(sub, sref.slug).await?;
        space_path::validate_rel_path(sref.rel_path)?;
        let mut full = self.library.space_root(&space.slug);
        if !sref.rel_path.is_empty() {
            full.push(sref.rel_path);
        }
        Ok(full)
    }

    #[instrument(name = "library.space_fs.view_file", skip(self, sub))]
    pub async fn view_file(
        &self,
        sub: &AuthSubject,
        sref: SpaceRef<'_>,
        view_range: Option<(i64, i64)>,
    ) -> Result<String, ProjectError> {
        let full = self.resolve(sub, sref).await?;
        let metadata = tokio::fs::metadata(&full)
            .await
            .map_err(|e| io_err(format!("stat {}: {e}", full.display())))?;
        if metadata.is_dir() {
            return self.view_dir_at(&full).await;
        }
        if metadata.len() > MAX_VIEW_FILE_BYTES {
            return Err(io_err(format!(
                "file too large ({} bytes > {} cap); use `grep` to extract a slice",
                metadata.len(),
                MAX_VIEW_FILE_BYTES
            ))
            .into());
        }
        let content = tokio::fs::read_to_string(&full)
            .await
            .map_err(|e| io_err(format!("read {}: {e}", full.display())))?;
        Ok(apply_view_range(&content, view_range))
    }

    #[instrument(name = "library.space_fs.view_dir", skip(self, sub))]
    pub async fn view_dir(
        &self,
        sub: &AuthSubject,
        sref: SpaceRef<'_>,
    ) -> Result<String, ProjectError> {
        let full = self.resolve(sub, sref).await?;
        self.view_dir_at(&full).await
    }

    async fn view_dir_at(&self, full: &std::path::Path) -> Result<String, ProjectError> {
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
        Ok(names.join("\n"))
    }

    /// Glob via `rg --files -g <pattern>`, anchored at the space root.
    /// Matches the wire-shape used by the `Glob` top-level tool.
    #[instrument(name = "library.space_fs.glob", skip(self, sub))]
    pub async fn glob(
        &self,
        sub: &AuthSubject,
        sref: SpaceRef<'_>,
        pattern: &str,
    ) -> Result<String, ProjectError> {
        let full = self.resolve(sub, sref).await?;
        let mut cmd = Command::new("rg");
        cmd.arg("--files").arg("-g").arg(pattern).arg(&full);
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
        let rel: Vec<&str> = stdout
            .lines()
            .map(|l| l.strip_prefix(&prefix).unwrap_or(l))
            .collect();
        Ok(rel.join("\n"))
    }

    /// Grep via `rg`, anchored at the space root. Forwards a curated
    /// subset of flags that the `Grep` top-level tool exposes.
    #[instrument(name = "library.space_fs.grep", skip(self, sub, args))]
    pub async fn grep(
        &self,
        sub: &AuthSubject,
        sref: SpaceRef<'_>,
        args: &Value,
    ) -> Result<String, ProjectError> {
        let full = self.resolve(sub, sref).await?;
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
        if args.get("multiline").and_then(|v| v.as_bool()).unwrap_or(false) {
            cmd.arg("-U").arg("--multiline-dotall");
        }
        if let Some(g) = args.get("glob").and_then(|v| v.as_str()) {
            cmd.arg("-g").arg(g);
        }
        if let Some(t) = args.get("type").and_then(|v| v.as_str()) {
            cmd.arg("--type").arg(t);
        }

        cmd.arg("--").arg(pattern).arg(&full);

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
            return Ok(head.join("\n"));
        }
        Ok(rel)
    }
}

fn io_err(msg: String) -> super::space::SpaceError {
    super::space::SpaceError::Io(msg)
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
