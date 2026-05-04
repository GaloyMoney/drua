//! Shared helpers for inspect-style tool dispatch:
//! - `InspectTool` enum (Read|Ls|Grep|Glob), shared by sandbox and
//!   space inspect commands across top-level and admin toolsets.
//! - `parse_view_range`: zero-based `{offset, limit}` → 1-based
//!   `(start, end)` view-range conversion. Also reused by sandbox
//!   `build_read_request` to construct the editor `view_range` arg.
//! - `dispatch_inspect` / `require_space_op`: space-specific helpers
//!   that funnel through `SpaceFs`.

use rmcp::model::{CallToolResult, Content, JsonObject};
use serde::Deserialize;

use drua_library::SpaceError;

use crate::auth::AuthSubject;
use crate::space_fs::{FileView, SpaceFs};

use super::error::ToolSetsError;

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InspectTool {
    Read,
    Ls,
    Grep,
    Glob,
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

/// Runs an `InspectTool` against `space:<slug>/<tool_args.path>`,
/// formatting the response as plain text. `Ok(None)` from `SpaceFs`
/// (only reachable for an empty slug, since callers always prefix
/// `space:`) is converted to an error so callers never see a silent
/// success.
pub(crate) async fn dispatch_inspect(
    space_fs: &SpaceFs,
    subject: &AuthSubject,
    slug: &str,
    tool: InspectTool,
    tool_args: JsonObject,
) -> Result<CallToolResult, ToolSetsError> {
    let path = tool_args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let space_path = format!("space:{slug}/{path}");
    let invalid = || -> ToolSetsError {
        ToolSetsError::Library(SpaceError::Io(format!("invalid space path: {space_path}")).into())
    };

    match tool {
        InspectTool::Read => {
            let view_range = parse_view_range(&tool_args);
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
        InspectTool::Ls => {
            let entries = space_fs
                .view_dir(subject, &space_path)
                .await?
                .ok_or_else(invalid)?;
            Ok(CallToolResult::success(vec![Content::text(
                entries.join("\n"),
            )]))
        }
        InspectTool::Glob => {
            let pattern = tool_args
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
        InspectTool::Grep => {
            let args = serde_json::Value::Object(tool_args);
            let out = space_fs
                .grep(subject, &space_path, &args)
                .await?
                .ok_or_else(invalid)?;
            Ok(CallToolResult::success(vec![Content::text(out)]))
        }
    }
}

/// Errors `Ok(None)` (empty slug → `parse_space_path` returns None)
/// as `InvalidArgument` so write/delete/move callers can't silently
/// no-op. Successful ops just propagate.
pub(crate) fn require_space_op(result: Option<()>, what: &str) -> Result<(), ToolSetsError> {
    if result.is_none() {
        return Err(ToolSetsError::InvalidArgument(format!(
            "slug must be non-empty for {what}"
        )));
    }
    Ok(())
}
