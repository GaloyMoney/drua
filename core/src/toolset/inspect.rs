//! Shared helpers for sub-discriminator tool dispatch:
//! - `ReadOp` (Read|Ls|Grep|Glob): consumed by `sandbox.inspect`
//!   (admin + top-level) and `spaces.view`.
//! - `EditOp` (Write|StrReplace|Insert|Delete|Move): consumed by
//!   `spaces.edit` (admin + top-level).
//! - `parse_view_range`: zero-based `{offset, limit}` → 1-based
//!   `(start, end)` view-range conversion. Reused by sandbox
//!   `build_read_request` to construct the editor `view_range` arg.
//! - `dispatch_view` / `dispatch_edit` / `require_space_op`: space
//!   helpers that funnel through `SpaceFs`.

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use drua_library::SpaceError;

use crate::auth::AuthSubject;
use crate::space_fs::{FileView, SpaceFs};

use super::error::ToolSetsError;

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReadOp {
    Read,
    Ls,
    Grep,
    Glob,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EditOp {
    Write,
    StrReplace,
    Insert,
    Delete,
    Move,
}

/// rev3 D17: which scheme a `spaces`/`drua_admin_spaces` `view`/`edit`
/// call addresses — the `target` field's counterpart to the
/// `space:`/`draft:` path prefix direct file-tool callers use.
/// Defaults to `Main` (today's behaviour) when omitted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SpaceTarget {
    #[default]
    Main,
    Draft,
}

impl SpaceTarget {
    fn scheme(self) -> &'static str {
        match self {
            SpaceTarget::Main => "space",
            SpaceTarget::Draft => "draft",
        }
    }
}

/// Translates `{offset, limit}` (zero-based) into the 1-based,
/// inclusive `(start, end)` range the file view layer expects.
/// `end == -1` means EOF.
pub(crate) fn parse_view_range(args: &JsonObject) -> Option<(i64, i64)> {
    let offset = args
        .get("offset")
        .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()));
    let limit = args
        .get("limit")
        .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()));
    if offset.is_none() && limit.is_none() {
        return None;
    }
    let start = offset.unwrap_or(0) + 1;
    let end = match limit {
        Some(l) => start + l - 1,
        None => -1,
    };
    Some((start, end))
}

/// `op_args.details` for the `ls`/`glob` sub-ops — always safe to read
/// here since `dispatch_view` only ever runs against a `space:` path.
fn op_args_details(args: &JsonObject) -> bool {
    args.get("details")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Runs a `ReadOp` against `space:<slug>/<op_args.path>`,
/// formatting the response as plain text. `Ok(None)` from `SpaceFs`
/// (only reachable for an empty slug, since callers always prefix
/// `space:`) is converted to an error so callers never see a silent
/// success.
pub(crate) async fn dispatch_view(
    space_fs: &SpaceFs,
    subject: &AuthSubject,
    slug: &str,
    op: ReadOp,
    op_args: JsonObject,
    target: SpaceTarget,
) -> Result<CallToolResult, ToolSetsError> {
    let path = op_args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let space_path = format!("{}:{slug}/{path}", target.scheme());
    let invalid = || -> ToolSetsError {
        ToolSetsError::Library(SpaceError::Io(format!("invalid space path: {space_path}")).into())
    };

    let text = match op {
        ReadOp::Read => {
            let view_range = parse_view_range(&op_args);
            let view = space_fs
                .view_file(subject, &space_path, view_range)
                .await?
                .ok_or_else(invalid)?;
            match view {
                FileView::File(text) => text,
                FileView::Dir(entries) => entries.join("\n"),
            }
        }
        ReadOp::Ls => {
            if op_args_details(&op_args) {
                let entries = space_fs
                    .view_dir_detailed(subject, &space_path)
                    .await?
                    .ok_or_else(invalid)?;
                let (_, text, _) = super::top_level::render_detailed(entries);
                text
            } else {
                let entries = space_fs
                    .view_dir(subject, &space_path)
                    .await?
                    .ok_or_else(invalid)?;
                entries.join("\n")
            }
        }
        ReadOp::Glob => {
            let pattern = op_args
                .get("pattern")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolSetsError::MissingArgument("pattern".to_string()))?;
            if op_args_details(&op_args) {
                let entries = space_fs
                    .glob_detailed(subject, &space_path, pattern)
                    .await?
                    .ok_or_else(invalid)?;
                let (_, text, _) = super::top_level::render_detailed(entries);
                text
            } else {
                let matches = space_fs
                    .glob(subject, &space_path, pattern)
                    .await?
                    .ok_or_else(invalid)?;
                matches.join("\n")
            }
        }
        ReadOp::Grep => {
            let args: sandbox::GrepInput =
                serde_json::from_value(serde_json::Value::Object(op_args))
                    .map_err(|e| ToolSetsError::InvalidArgument(e.to_string()))?;
            space_fs
                .grep(subject, &space_path, &args)
                .await?
                .ok_or_else(invalid)?
        }
    };

    Ok(CallToolResult::success(vec![Content::text(
        stamped_read(space_fs, subject, &space_path, text).await,
    )]))
}

/// D10/D19/§5.3: prefixes `text` with the resolved stamp for a
/// `view`-side `space_path`, so every `spaces`/`drua_admin_spaces`
/// read carries it too — not just the direct file tools' own
/// `SpaceFs::resolve`-backed callers. Read-only: `dispatch_edit`
/// doesn't use this — its stamp comes back from the write call itself,
/// since a second `resolve` here would see the draft a lazy write just
/// created and under-report it (bugbot 2026-09-26). Falls back to
/// `text` unstamped if the resolve somehow fails a second time — the
/// op itself already succeeded, so a stamp lookup failure shouldn't
/// turn a successful result into an error.
async fn stamped_read(
    space_fs: &SpaceFs,
    subject: &AuthSubject,
    space_path: &str,
    text: String,
) -> String {
    match space_fs.resolved_stamp(subject, space_path, false).await {
        Ok(Some(stamp)) => format!("{stamp}\n{text}"),
        _ => text,
    }
}

/// Runs an `EditOp` against the relevant `space:<slug>/...` path(s).
/// Per-op required args:
/// - `write`: `path`, `content`
/// - `str_replace`: `path`, `old_str`, `new_str`
/// - `insert`: `path`, `line` (1-based; insert AFTER; `0` prepends), `text`
/// - `delete`: `path`
/// - `move`: `from`, `to`
pub(crate) async fn dispatch_edit(
    space_fs: &SpaceFs,
    subject: &AuthSubject,
    slug: &str,
    op: EditOp,
    op_args: JsonObject,
    target: SpaceTarget,
) -> Result<CallToolResult, ToolSetsError> {
    let scheme = target.scheme();
    let str_arg = |key: &str| -> Result<String, ToolSetsError> {
        op_args
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| ToolSetsError::MissingArgument(key.to_string()))
    };
    let int_arg = |key: &str| -> Result<i64, ToolSetsError> {
        op_args
            .get(key)
            .and_then(|v| v.as_i64())
            .ok_or_else(|| ToolSetsError::MissingArgument(key.to_string()))
    };

    // Each arm's `stamp` comes from the very `resolve` that performed
    // the write, not a second one after the fact — re-resolving here
    // would see the draft a lazy first write just created and report
    // `just_started: false`, losing the "started" stamp (bugbot
    // 2026-09-26; see `SpaceFs::write_file`'s doc).
    let (text, stamp) = match op {
        EditOp::Write => {
            let path = str_arg("path")?;
            let content = str_arg("content")?;
            let space_path = format!("{scheme}:{slug}/{path}");
            let result = space_fs.write_file(subject, &space_path, content).await?;
            let stamp = require_space_op(result, "write")?;
            (format!("Wrote {space_path}"), stamp)
        }
        EditOp::StrReplace => {
            let path = str_arg("path")?;
            let old_str = str_arg("old_str")?;
            let new_str = str_arg("new_str")?;
            let space_path = format!("{scheme}:{slug}/{path}");
            let result = space_fs
                .str_replace(subject, &space_path, old_str, new_str)
                .await?;
            let stamp = require_space_op(result, "str_replace")?;
            (format!("Replaced in {space_path}"), stamp)
        }
        EditOp::Insert => {
            let path = str_arg("path")?;
            let line = int_arg("line")?;
            if line < 0 {
                return Err(ToolSetsError::InvalidArgument(
                    "line must be >= 0".to_string(),
                ));
            }
            let text = str_arg("text")?;
            let space_path = format!("{scheme}:{slug}/{path}");
            let result = space_fs
                .insert_line(subject, &space_path, line as usize, text)
                .await?;
            let stamp = require_space_op(result, "insert")?;
            (format!("Inserted into {space_path}"), stamp)
        }
        EditOp::Delete => {
            let path = str_arg("path")?;
            let space_path = format!("{scheme}:{slug}/{path}");
            let result = space_fs.delete_file(subject, &space_path).await?;
            let stamp = require_space_op(result, "delete")?;
            (format!("Deleted {space_path}"), stamp)
        }
        EditOp::Move => {
            let from = str_arg("from")?;
            let to = str_arg("to")?;
            let from_path = format!("{scheme}:{slug}/{from}");
            let to_path = format!("{scheme}:{slug}/{to}");
            let result = space_fs.move_file(subject, &from_path, &to_path).await?;
            let stamp = require_space_op(result, "move")?;
            (format!("Moved {from_path} -> {to_path}"), stamp)
        }
    };

    Ok(CallToolResult::success(vec![Content::text(format!(
        "{stamp}\n{text}"
    ))]))
}

/// Errors `Ok(None)` (empty slug → `parse_space_path` returns None)
/// as `InvalidArgument` so callers can't silently no-op. Successful
/// ops just propagate.
pub(crate) fn require_space_op<T>(result: Option<T>, what: &str) -> Result<T, ToolSetsError> {
    result
        .ok_or_else(|| ToolSetsError::InvalidArgument(format!("slug must be non-empty for {what}")))
}
