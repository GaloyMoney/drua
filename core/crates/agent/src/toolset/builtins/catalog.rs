//! Catalog-backed meta-tools: `search_tools`, `describe_tool`, and
//! `call_tool`. All three operate against the shared [`Catalog`] — the first
//! two are read-only and skip auth/audit, while `call_tool` runs the full
//! scope-check + audit path via
//! [`dispatch_tool_call`](super::super::dispatch_tool_call).

use std::sync::{Arc, LazyLock, RwLock};

use rmcp::model::{CallToolResult, Content, JsonObject};

use super::super::catalog::Catalog;
use super::super::error::ToolSetsError;
use super::super::filter::OutputFilter;
use super::super::traits::{SearchableToolSet, TopLevelTool};

// ---------------------------------------------------------------------------
// search_tools
// ---------------------------------------------------------------------------

pub struct SearchCatalog {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
}

impl SearchCatalog {
    pub fn new(sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>) -> Self {
        Self { sets }
    }

    fn catalog(&self) -> Catalog {
        Catalog::new(Arc::clone(&self.sets))
    }
}

static SEARCH_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Free-form search query" },
            "category": { "type": "string", "description": "Optional category filter ('all' for any)" }
        }
    })
});

#[async_trait::async_trait]
impl TopLevelTool for SearchCatalog {
    fn name(&self) -> &str {
        "search_tools"
    }
    fn description(&self) -> &str {
        "Search for available tools across all upstream services. Returns tool \
         names, brief descriptions, and categories. Use this first to find \
         relevant tools before calling them."
    }
    fn input_schema(&self) -> &serde_json::Value {
        &SEARCH_SCHEMA
    }

    async fn call(&self, arguments: Option<JsonObject>) -> Result<CallToolResult, ToolSetsError> {
        let args = arguments.as_ref();
        let query = args.and_then(|a| a.get("query")).and_then(|v| v.as_str());
        let category = args
            .and_then(|a| a.get("category"))
            .and_then(|v| v.as_str());
        let results = self.catalog().search(query, category).await;
        let text = Catalog::format_search_results(&results);
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

// ---------------------------------------------------------------------------
// describe_tool
// ---------------------------------------------------------------------------

pub struct DescribeCatalogTool {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
}

impl DescribeCatalogTool {
    pub fn new(sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>) -> Self {
        Self { sets }
    }

    fn catalog(&self) -> Catalog {
        Catalog::new(Arc::clone(&self.sets))
    }
}

static DESCRIBE_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "tool_name": { "type": "string", "description": "The prefixed tool name returned from search_tools" }
        },
        "required": ["tool_name"]
    })
});

#[async_trait::async_trait]
impl TopLevelTool for DescribeCatalogTool {
    fn name(&self) -> &str {
        "describe_tool"
    }
    fn description(&self) -> &str {
        "Get the full parameter schema and detailed description for a specific \
         tool. Use after search_tools to understand how to call a tool."
    }
    fn input_schema(&self) -> &serde_json::Value {
        &DESCRIBE_SCHEMA
    }

    async fn call(&self, arguments: Option<JsonObject>) -> Result<CallToolResult, ToolSetsError> {
        let tool_name = arguments
            .as_ref()
            .and_then(|a| a.get("tool_name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let text = match self.catalog().describe(tool_name).await {
            Some(entry) => Catalog::format_describe(&entry),
            None => format!("Tool not found: {tool_name}"),
        };
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

// ---------------------------------------------------------------------------
// call_tool
// ---------------------------------------------------------------------------

/// The call-a-prefixed-tool meta-tool. Unlike `search_tools` / `describe_tool`,
/// this one mutates upstream services, so it threads `auth` through and uses
/// the full [`ToolSets::call_with_filter`](super::super::ToolSets::call_with_filter)
/// path — scope checks + audit recording + the caller-supplied output filter.
pub struct CallCatalogTool {
    sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>,
}

impl CallCatalogTool {
    pub fn new(sets: Arc<RwLock<Vec<Arc<dyn SearchableToolSet>>>>) -> Self {
        Self { sets }
    }
}

static CALL_SCHEMA: LazyLock<serde_json::Value> = LazyLock::new(|| {
    serde_json::json!({
        "type": "object",
        "properties": {
            "tool_name": {
                "type": "string",
                "description": "The prefixed tool name returned from search_tools (e.g. 'honeycomb_list_environments')"
            },
            "arguments": {
                "type": "object",
                "description": "Tool arguments matching the schema from describe_tool"
            },
            "output_filter": {
                "type": "object",
                "description": "Optional post-processing filter applied to the tool's output (head / tail / grep / invert_match / context_lines). Falls back to the tool's default or the global default when omitted."
            }
        },
        "required": ["tool_name"]
    })
});

#[async_trait::async_trait]
impl TopLevelTool for CallCatalogTool {
    fn name(&self) -> &str {
        "call_tool"
    }
    fn description(&self) -> &str {
        "Execute an upstream tool by its prefixed name with the provided \
         arguments. Use describe_tool first to understand the parameters. \
         Supports an optional output_filter to trim large outputs."
    }
    fn input_schema(&self) -> &serde_json::Value {
        &CALL_SCHEMA
    }

    /// Authorization is delegated to the inner toolset: look up the requested
    /// `tool_name`, fetch its `required_scopes`, and check those against the
    /// caller's scopes. If the inner tool can't be found we let the call
    /// proceed and surface the not-found error there.
    fn is_authorized(&self, scopes: &[&str], arguments: Option<&JsonObject>) -> bool {
        let Some(tool_name) = arguments
            .and_then(|a| a.get("tool_name"))
            .and_then(|v| v.as_str())
        else {
            return false;
        };
        let catalog = Catalog::new(Arc::clone(&self.sets));
        let Some((set, _, _)) = catalog.find_set(tool_name) else {
            // Unknown tool — authorize so the call() path can return a
            // structured ToolNotFound error.
            return true;
        };
        let required = set.required_scopes();
        if required.is_empty() {
            return true;
        }
        required.iter().all(|s| scopes.contains(s))
    }

    async fn call(&self, arguments: Option<JsonObject>) -> Result<CallToolResult, ToolSetsError> {
        let mut args = arguments.unwrap_or_default();
        let tool_name = args
            .remove("tool_name")
            .and_then(|v| v.as_str().map(str::to_string))
            .ok_or_else(|| ToolSetsError::ToolNotFound("missing tool_name".to_string()))?;
        let inner_args = args.remove("arguments").and_then(|v| match v {
            serde_json::Value::Object(obj) => Some(obj),
            _ => None,
        });
        let output_filter: Option<OutputFilter> = args
            .remove("output_filter")
            .and_then(|v| serde_json::from_value(v).ok());

        let catalog = Catalog::new(Arc::clone(&self.sets));
        let (set, name, tool_default_filter) = catalog
            .find_set(&tool_name)
            .ok_or_else(|| ToolSetsError::ToolNotFound(tool_name.clone()))?;

        let result = set.call(&name, inner_args, None).await;
        let filter = output_filter
            .or(tool_default_filter)
            .unwrap_or_else(OutputFilter::global_default);
        result.and_then(|r| filter.apply(r))
    }
}
