//! Built-in [`TopLevelTool`](super::traits::TopLevelTool) implementations.

use rmcp::model::JsonObject;

use super::error::ToolSetsError;

/// Serde helpers for liberal deserialization of MCP tool arguments.
///
/// Agents sometimes send values in unexpected representations — e.g.
/// `"20"` instead of `20` for an integer field. These helpers accept
/// both native JSON types and their stringified equivalents.
mod liberal {
    use serde::Deserialize;

    /// Deserialize an `i64` from either a JSON number or a string like `"20"`.
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

    /// Deserialize an `Option<i64>` from a JSON number, string, or null.
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

/// Deserialize tool arguments into a typed params struct.
///
/// Converts the raw `Option<JsonObject>` from [`TopLevelTool::call`] into `T`.
/// Missing arguments are treated as an empty object (suitable for tools with
/// all-optional fields).
pub(super) fn parse_params<T: serde::de::DeserializeOwned>(
    arguments: Option<JsonObject>,
) -> Result<T, ToolSetsError> {
    let value = serde_json::Value::Object(arguments.unwrap_or_default());
    serde_json::from_value(value).map_err(|e| ToolSetsError::InvalidArgument(e.to_string()))
}

/// Derive a JSON Schema from a params struct that implements [`schemars::JsonSchema`].
///
/// Uses draft-07, inlines sub-schemas, and strips `title`/`definitions`/`$schema`
/// to match the shape expected by MCP tool `input_schema`. Adds
/// `additionalProperties: false` so the schema rejects unknown fields without
/// needing `#[serde(deny_unknown_fields)]` (which conflicts with `#[serde(flatten)]`).
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
        obj.remove("definitions");
        obj.insert(
            "additionalProperties".into(),
            serde_json::Value::Bool(false),
        );
    }
    value
}

mod agent;
mod bash;
mod catalog;
mod glob;
mod grep;
mod inspect;
mod log;
mod ls;
mod read;
mod sandbox;
mod text_editor;
mod whoami;

pub use agent::{
    WorkspaceAgentAttachSandbox, WorkspaceAgentCreate, WorkspaceAgentDetachSandbox,
    WorkspaceListAgents,
};
pub use bash::Bash;
pub use catalog::{CallCatalogTool, DescribeCatalogTool, SearchCatalog};
pub use glob::GlobTool;
pub use grep::Grep;
pub use inspect::WorkspaceInspectSandbox;
pub use log::WorkspaceLog;
pub use ls::Ls;
pub use read::Read;
pub use sandbox::{WorkspaceCreateSandbox, WorkspaceGetSandbox, WorkspaceListSandboxes};
pub use text_editor::TextEditor;
pub use whoami::WhoAmI;
