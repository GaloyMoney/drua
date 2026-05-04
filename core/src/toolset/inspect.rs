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
) -> Result<CallToolResult, ToolSetsError> {
    let path = op_args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let space_path = format!("space:{slug}/{path}");
    let invalid = || -> ToolSetsError {
        ToolSetsError::Library(SpaceError::Io(format!("invalid space path: {space_path}")).into())
    };

    match op {
        ReadOp::Read => {
            let view_range = parse_view_range(&op_args);
            let view = space_fs
                .view_file(subject, &space_path, view_range)
                .await?
                .ok_or_else(invalid)?;
            let text = match view {
                FileView::File(text) => text,
                FileView::Dir(entries) => entries.join("\n"),
            };
            Ok(CallToolResult::success(vec![Content::text(text)]))
        }
        ReadOp::Ls => {
            let entries = space_fs
                .view_dir(subject, &space_path)
                .await?
                .ok_or_else(invalid)?;
            Ok(CallToolResult::success(vec![Content::text(
                entries.join("\n"),
            )]))
        }
        ReadOp::Glob => {
            let pattern = op_args
                .get("pattern")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolSetsError::MissingArgument("pattern".to_string()))?;
            let matches = space_fs
                .glob(subject, &space_path, pattern)
                .await?
                .ok_or_else(invalid)?;
            Ok(CallToolResult::success(vec![Content::text(
                matches.join("\n"),
            )]))
        }
        ReadOp::Grep => {
            let args = serde_json::Value::Object(op_args);
            let out = space_fs
                .grep(subject, &space_path, &args)
                .await?
                .ok_or_else(invalid)?;
            Ok(CallToolResult::success(vec![Content::text(out)]))
        }
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
) -> Result<CallToolResult, ToolSetsError> {
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

    match op {
        EditOp::Write => {
            let path = str_arg("path")?;
            let content = str_arg("content")?;
            let space_path = format!("space:{slug}/{path}");
            let result = space_fs.write_file(subject, &space_path, content).await?;
            require_space_op(result, "write")?;
            Ok(CallToolResult::success(vec![Content::text(format!(
                "Wrote {space_path}"
            ))]))
        }
        EditOp::StrReplace => {
            let path = str_arg("path")?;
            let old_str = str_arg("old_str")?;
            let new_str = str_arg("new_str")?;
            let space_path = format!("space:{slug}/{path}");
            let result = space_fs
                .str_replace(subject, &space_path, old_str, new_str)
                .await?;
            require_space_op(result, "str_replace")?;
            Ok(CallToolResult::success(vec![Content::text(format!(
                "Replaced in {space_path}"
            ))]))
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
            let space_path = format!("space:{slug}/{path}");
            let result = space_fs
                .insert_line(subject, &space_path, line as usize, text)
                .await?;
            require_space_op(result, "insert")?;
            Ok(CallToolResult::success(vec![Content::text(format!(
                "Inserted into {space_path}"
            ))]))
        }
        EditOp::Delete => {
            let path = str_arg("path")?;
            let space_path = format!("space:{slug}/{path}");
            let result = space_fs.delete_file(subject, &space_path).await?;
            require_space_op(result, "delete")?;
            Ok(CallToolResult::success(vec![Content::text(format!(
                "Deleted {space_path}"
            ))]))
        }
        EditOp::Move => {
            let from = str_arg("from")?;
            let to = str_arg("to")?;
            let from_path = format!("space:{slug}/{from}");
            let to_path = format!("space:{slug}/{to}");
            let result = space_fs.move_file(subject, &from_path, &to_path).await?;
            require_space_op(result, "move")?;
            Ok(CallToolResult::success(vec![Content::text(format!(
                "Moved {from_path} -> {to_path}"
            ))]))
        }
    }
}

/// Errors `Ok(None)` (empty slug → `parse_space_path` returns None)
/// as `InvalidArgument` so callers can't silently no-op. Successful
/// ops just propagate.
pub(crate) fn require_space_op(result: Option<()>, what: &str) -> Result<(), ToolSetsError> {
    if result.is_none() {
        return Err(ToolSetsError::InvalidArgument(format!(
            "slug must be non-empty for {what}"
        )));
    }
    Ok(())
}
