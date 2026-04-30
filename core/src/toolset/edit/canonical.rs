//! Canonical edit-operation types — the lingua franca that every
//! [`ProviderEditAdapter`](super::adapter::ProviderEditAdapter) parses model
//! tool-calls into. Wire-compatible with Anthropic's `text_editor_*` tool for
//! the four overlapping commands (`view`, `create`, `str_replace`, `insert`)
//! so a serialized [`EditOp`] can be forwarded verbatim to the
//! sandbox-tool-server's `/execute` endpoint.
//!
//! Two extras (`delete`, `rename`) are required to losslessly model
//! OpenAI V4A `apply_patch` and Gemini-style file moves; the Anthropic
//! adapter never emits them.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum EditOp {
    View {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        view_range: Option<[i64; 2]>,
    },
    Create {
        path: String,
        file_text: String,
    },
    StrReplace {
        path: String,
        old_str: String,
        new_str: String,
    },
    Insert {
        path: String,
        insert_line: u64,
        new_str: String,
    },
    Delete {
        path: String,
    },
    Rename {
        from: String,
        to: String,
    },
}

impl EditOp {
    /// Stable name for the discriminator used in tracing / audit.
    pub fn command(&self) -> &'static str {
        match self {
            EditOp::View { .. } => "view",
            EditOp::Create { .. } => "create",
            EditOp::StrReplace { .. } => "str_replace",
            EditOp::Insert { .. } => "insert",
            EditOp::Delete { .. } => "delete",
            EditOp::Rename { .. } => "rename",
        }
    }

    /// Path the op reads/writes. For [`EditOp::Rename`] this is the
    /// destination path (the canonical "target" of the operation).
    pub fn path(&self) -> &str {
        match self {
            EditOp::View { path, .. }
            | EditOp::Create { path, .. }
            | EditOp::StrReplace { path, .. }
            | EditOp::Insert { path, .. }
            | EditOp::Delete { path } => path,
            EditOp::Rename { to, .. } => to,
        }
    }

    /// Read-only ops can run on read-only sandbox attachments.
    pub fn is_read_only(&self) -> bool {
        matches!(self, EditOp::View { .. })
    }
}

#[derive(Debug, Clone)]
pub enum EditOpResult {
    /// Numbered-line file contents or directory listing.
    View {
        output: String,
    },
    Created {
        path: String,
    },
    Replaced {
        path: String,
        occurrences: usize,
    },
    Inserted {
        path: String,
        line: u64,
    },
    Deleted {
        path: String,
    },
    Renamed {
        from: String,
        to: String,
    },
}

impl EditOpResult {
    pub fn human_summary(&self) -> String {
        match self {
            EditOpResult::View { output } => output.clone(),
            EditOpResult::Created { path } => format!("Created {path}"),
            EditOpResult::Replaced { path, occurrences } => {
                format!("Replaced {occurrences} occurrence(s) in {path}")
            }
            EditOpResult::Inserted { path, line } => {
                format!("Inserted at line {line} in {path}")
            }
            EditOpResult::Deleted { path } => format!("Deleted {path}"),
            EditOpResult::Renamed { from, to } => format!("Renamed {from} → {to}"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EditParseError {
    #[error("missing required field: {0}")]
    MissingField(&'static str),
    #[error("unknown command: {0}")]
    UnknownCommand(String),
    #[error("invalid argument shape: {0}")]
    InvalidShape(String),
    #[error("malformed V4A patch: {0}")]
    MalformedPatch(String),
    #[error("ambiguous hunk: {0}")]
    AmbiguousHunk(String),
}

#[derive(Debug, thiserror::Error)]
pub enum EditExecError {
    #[error("file not found: {0}")]
    NotFound(String),
    #[error("`old_str` matched {actual} times in {path}; expected exactly 1")]
    NonUniqueMatch { path: String, actual: usize },
    #[error("sandbox execution error: {0}")]
    Sandbox(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_serialises_with_anthropic_field_names() {
        let op = EditOp::View {
            path: "/w/a.rs".to_string(),
            view_range: Some([10, -1]),
        };
        let v = serde_json::to_value(&op).unwrap();
        assert_eq!(v["command"], "view");
        assert_eq!(v["path"], "/w/a.rs");
        assert_eq!(v["view_range"], serde_json::json!([10, -1]));
    }

    #[test]
    fn str_replace_round_trips() {
        let op = EditOp::StrReplace {
            path: "/w/a.rs".to_string(),
            old_str: "let x = 1;".to_string(),
            new_str: "let x = 2;".to_string(),
        };
        let v = serde_json::to_value(&op).unwrap();
        let back: EditOp = serde_json::from_value(v).unwrap();
        assert_eq!(op, back);
    }

    #[test]
    fn insert_uses_new_str_field() {
        let op = EditOp::Insert {
            path: "/w/a.rs".to_string(),
            insert_line: 0,
            new_str: "// header\n".to_string(),
        };
        let v = serde_json::to_value(&op).unwrap();
        assert_eq!(v["new_str"], "// header\n");
        assert_eq!(v["insert_line"], 0);
    }

    #[test]
    fn delete_and_rename_serialise() {
        let d = EditOp::Delete {
            path: "old.rs".to_string(),
        };
        let r = EditOp::Rename {
            from: "a.rs".to_string(),
            to: "b.rs".to_string(),
        };
        assert_eq!(serde_json::to_value(&d).unwrap()["command"], "delete");
        assert_eq!(serde_json::to_value(&r).unwrap()["command"], "rename");
    }

    #[test]
    fn unknown_command_errors() {
        let v = serde_json::json!({"command": "exec", "path": "/w/a.rs"});
        let r: Result<EditOp, _> = serde_json::from_value(v);
        assert!(r.is_err());
    }

    #[test]
    fn is_read_only_only_for_view() {
        let view = EditOp::View {
            path: "p".to_string(),
            view_range: None,
        };
        let create = EditOp::Create {
            path: "p".to_string(),
            file_text: String::new(),
        };
        assert!(view.is_read_only());
        assert!(!create.is_read_only());
    }
}
