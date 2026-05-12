//! Bash preprocessor — a placeholder for tool-name dispatch. Today
//! the bash text channel is already the merged stdout+stderr form
//! the upstream emits, so we just passthrough and let the generic
//! chain compact runs (cargo / nix / rsync / git patterns all fire
//! inside bash output too).
//!
//! Reserved here so adding bash-specific preprocessing later (e.g.
//! highlighting failed steps, summarising exit codes) doesn't need
//! to re-touch the walker dispatch table.

use super::{identity_mapping, Preprocessed};

pub const TOOL_NAMES: &[&str] = &["bash"];

pub fn run(raw: &str) -> Preprocessed {
    Preprocessed {
        text: raw.to_string(),
        preprocessed_to_raw: identity_mapping(raw),
    }
}
