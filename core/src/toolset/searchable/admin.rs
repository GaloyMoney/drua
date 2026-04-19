//! `AdminToolSet` — wraps all admin-only [`TopLevelTool`] impls behind a
//! single [`SearchableToolSet`] so they live in the catalog (`search_tools`
//! → `describe_tool` → `call_tool`) instead of cluttering `list_tools`.
//!
//! Each tool keeps its existing `TopLevelTool` implementation unchanged.
//! The adapter strips the `admin_` prefix from each tool's name to produce
//! the short catalog name, then the `SearchableToolSet` prefix
//! (`drua_`) is added at query time.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::{CallToolResult, JsonObject};

use crate::auth::AuthSubject;

use super::super::error::ToolSetsError;
use super::super::traits::{SearchableToolSet, ToolSetEntry, TopLevelTool};

pub struct AdminToolSet {
    tools: Vec<ToolSetEntry>,
    impls: HashMap<String, Arc<dyn TopLevelTool>>,
}

impl AdminToolSet {
    pub fn new(tools: Vec<Arc<dyn TopLevelTool>>) -> Self {
        let mut entries = Vec::with_capacity(tools.len());
        let mut impls = HashMap::with_capacity(tools.len());

        for tool in tools {
            let short_name = tool
                .name()
                .strip_prefix("admin_")
                .unwrap_or(tool.name())
                .to_string();

            entries.push(ToolSetEntry {
                name: short_name.clone(),
                description: rmcp::model::Tool::new(
                    short_name.clone(),
                    tool.description().to_string(),
                    serde_json::from_value::<JsonObject>(tool.input_schema().clone())
                        .unwrap_or_default(),
                ),
                default_output_filter: None,
            });

            impls.insert(short_name, tool);
        }

        Self {
            tools: entries,
            impls,
        }
    }
}

#[async_trait::async_trait]
impl SearchableToolSet for AdminToolSet {
    fn name(&self) -> &str {
        "drua"
    }

    fn category(&self) -> &str {
        "admin"
    }

    fn category_description(&self) -> &str {
        "Workspace, agent, and sandbox management (admin)"
    }

    fn tools(&self) -> &[ToolSetEntry] {
        &self.tools
    }

    fn is_visible(&self, subject: &AuthSubject) -> bool {
        subject.is_admin()
    }

    async fn call(
        &self,
        subject: &AuthSubject,
        tool_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let tool = self
            .impls
            .get(tool_name)
            .ok_or_else(|| ToolSetsError::ToolNotFound(tool_name.to_string()))?;

        tool.call(subject, arguments).await
    }
}
