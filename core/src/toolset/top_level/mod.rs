//! Built-in [`TopLevelTool`](super::traits::TopLevelTool) implementations.

use rmcp::model::JsonObject;

use super::error::ToolSetsError;

/// Liberal deserialization helpers — agents sometimes send `"20"` instead of `20`.
mod liberal {
    use serde::Deserialize;

    pub(crate) fn deserialize_i64<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<i64, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum StringOrInt {
            Int(i64),
            Str(String),
        }
        match StringOrInt::deserialize(deserializer)? {
            StringOrInt::Int(v) => Ok(v),
            StringOrInt::Str(s) => s.parse().map_err(serde::de::Error::custom),
        }
    }

    pub(crate) fn deserialize_option_i64<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<i64>, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum StringOrInt {
            Int(i64),
            Str(String),
        }
        let opt: Option<StringOrInt> = Option::deserialize(deserializer)?;
        match opt {
            None => Ok(None),
            Some(StringOrInt::Int(v)) => Ok(Some(v)),
            Some(StringOrInt::Str(s)) => s.parse().map(Some).map_err(serde::de::Error::custom),
        }
    }
}

/// Missing arguments are treated as an empty object.
pub(super) fn parse_params<T: serde::de::DeserializeOwned>(
    arguments: Option<JsonObject>,
) -> Result<T, ToolSetsError> {
    let value = serde_json::Value::Object(arguments.unwrap_or_default());
    serde_json::from_value(value).map_err(|e| ToolSetsError::InvalidArgument(e.to_string()))
}

/// Draft-07, inlined sub-schemas, `additionalProperties: false` (avoids
/// `#[serde(deny_unknown_fields)]` which conflicts with `#[serde(flatten)]`).
/// `definitions` is **kept** so the compose TS generator can resolve `$ref`s
/// for recursive shapes (schemars 0.8 falls back to `$ref` even with
/// `inline_subschemas=true`).
pub(super) fn schema_for<T: schemars::JsonSchema>() -> serde_json::Value {
    let settings = schemars::gen::SchemaSettings::draft07().with(|s| {
        s.inline_subschemas = true;
        s.meta_schema = None;
    });
    let generator = settings.into_generator();
    let schema = generator.into_root_schema_for::<T>();
    let mut value = serde_json::to_value(schema).expect("schema serialization");
    if let Some(obj) = value.as_object_mut() {
        obj.remove("title");
        obj.insert(
            "additionalProperties".into(),
            serde_json::Value::Bool(false),
        );
    }
    value
}

// Shared output structs: schemars derives `output_schema()`, serde::Serialize
// produces `structured_content` at call time.

#[derive(serde::Serialize, schemars::JsonSchema)]
pub(super) struct TextOutput {
    pub output: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub(super) struct ContentOutput {
    pub content: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub(super) struct FilesOutput {
    pub files: Vec<String>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
pub(super) struct EntriesOutput {
    /// Directory entries (directories have trailing '/').
    pub entries: Vec<String>,
}

mod agent;
mod bash;
mod catalog;
mod compose;
mod compose_types;
mod glob;
mod grep;
mod log;
mod ls;
mod notes;
mod read;
mod sandbox;
mod skill;
mod text_editor;
mod use_skill;
mod whoami;
mod workflow;

pub use agent::WorkspaceAgent;
pub use bash::Bash;
pub use catalog::{CallCatalogTool, DescribeCatalogTool, SearchCatalog};
pub use compose::ComposeTool;
pub use compose_types::ComposeTypes;
pub use glob::GlobTool;
pub use grep::Grep;
pub use log::WorkspaceLog;
pub use ls::Ls;
pub use notes::NotesTool;
pub use read::Read;
pub use sandbox::WorkspaceSandbox;
pub use skill::SkillTool;
pub use text_editor::TextEditor;
pub use use_skill::UseSkillTool;
pub use whoami::WhoAmI;
pub use workflow::WorkflowTool;
