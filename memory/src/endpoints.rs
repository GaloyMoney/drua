use rmcp::model::{CallToolResult, Content};
use rmcp::schemars;

use crate::memory::NewMemory;
use crate::service::MemoryService;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct StoreMemoryParams {
    /// Descriptive title for the memory
    #[schemars(description = "Descriptive title for the memory")]
    title: String,

    /// Full content (findings, decisions, patterns, etc.)
    #[schemars(description = "Full content (findings, decisions, patterns, etc.)")]
    content: String,

    /// Lowercase tags for categorization
    #[schemars(description = "Lowercase tags for categorization")]
    tags: Option<Vec<String>>,

    /// Project scope for this memory
    #[schemars(description = "Project scope for this memory")]
    project: Option<String>,

    /// 'memory' (default) or 'report'
    #[schemars(description = "'memory' (default) or 'report'")]
    memory_type: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchMemoryParams {
    /// Search query (keywords or natural language)
    #[schemars(description = "Search query (keywords or natural language)")]
    query: String,

    /// Filter results by project
    #[schemars(description = "Filter results by project")]
    project: Option<String>,

    /// Maximum number of results to return (default: 10)
    #[schemars(description = "Maximum number of results to return (default: 10)")]
    limit: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ListMemoriesParams {
    /// Filter by project
    #[schemars(description = "Filter by project")]
    project: Option<String>,

    /// Filter by type: 'memory' or 'report'
    #[schemars(description = "Filter by type: 'memory' or 'report'")]
    memory_type: Option<String>,

    /// Maximum number of results to return (default: 20)
    #[schemars(description = "Maximum number of results to return (default: 20)")]
    limit: Option<u64>,
}

/// Facade for memory MCP endpoints, to be held by the MCP gateway.
#[derive(Debug, Clone)]
pub struct MemoryEndpoints {
    service: Option<MemoryService>,
}

impl MemoryEndpoints {
    pub fn new(service: MemoryService) -> Self {
        Self {
            service: Some(service),
        }
    }

    /// Create a disabled instance (when `db_path` is empty).
    pub fn disabled() -> Self {
        Self { service: None }
    }

    fn require_service(&self) -> Result<&MemoryService, rmcp::ErrorData> {
        self.service.as_ref().ok_or_else(|| {
            rmcp::ErrorData::new(
                rmcp::model::ErrorCode::INTERNAL_ERROR,
                "Memory service is disabled (no db_path configured)",
                None::<serde_json::Value>,
            )
        })
    }

    /// Execute a `store_memory` call.
    pub async fn store_memory(
        &self,
        params: StoreMemoryParams,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let service = self.require_service()?;
        let tags = params
            .tags
            .unwrap_or_default()
            .into_iter()
            .map(|t| t.to_lowercase())
            .collect::<Vec<_>>();

        // Treat memory_type=report as persistent.
        let persistent = params.memory_type.as_deref() == Some("report");

        let new = NewMemory {
            title: params.title.clone(),
            content: params.content,
            tags: tags.clone(),
            project: params.project,
            source_task: None,
            source_type: "agent".to_string(),
            persistent,
        };

        match service.store(new).await {
            Ok(memory) => {
                let short_id = &memory.id[..8.min(memory.id.len())];
                let tags_display = if tags.is_empty() {
                    String::from("(none)")
                } else {
                    tags.join(", ")
                };
                let label = if memory.persistent {
                    "persistent memory"
                } else {
                    "memory"
                };
                let text = format!(
                    "Stored {label}: \"{}\" (id: {short_id})\nTags: {tags_display}",
                    params.title
                );
                Ok(CallToolResult::success(vec![Content::text(text)]))
            }
            Err(e) => Err(rmcp::ErrorData::internal_error(
                format!("Failed to store memory: {e}"),
                None,
            )),
        }
    }

    /// Execute a `search_memory` query.
    pub async fn search_memory(
        &self,
        params: SearchMemoryParams,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let service = self.require_service()?;
        let limit = params.limit.unwrap_or(10) as usize;
        let project = params.project.as_deref();

        match service.search(&params.query, project, limit).await {
            Ok(results) => {
                if results.is_empty() {
                    return Ok(CallToolResult::success(vec![Content::text(
                        "No results found.",
                    )]));
                }

                let mut text = format!("## Results ({} found)\n", results.len());

                for (i, r) in results.iter().enumerate() {
                    text.push_str(&format!(
                        "\n### {}. {} (score: {:.2}, decay: {:.0}%",
                        i + 1,
                        r.title,
                        r.score,
                        r.decay_factor * 100.0,
                    ));
                    if r.persistent {
                        text.push_str(", persistent");
                    }
                    if r.pinned {
                        text.push_str(", pinned");
                    }
                    text.push(')');

                    if !r.tags.is_empty() {
                        text.push_str(&format!("\nTags: {}", r.tags.join(", ")));
                    }
                    if let Some(ref proj) = r.project {
                        text.push_str(&format!("\nProject: {proj}"));
                    }

                    // Content snippet (first 300 chars).
                    let snippet: String = r.content.chars().take(300).collect();
                    text.push_str(&format!("\n\n{snippet}"));
                    if r.content.len() > 300 {
                        text.push_str("...");
                    }
                    text.push_str("\n\n---");
                }

                Ok(CallToolResult::success(vec![Content::text(text)]))
            }
            Err(e) => Err(rmcp::ErrorData::internal_error(
                format!("Search failed: {e}"),
                None,
            )),
        }
    }

    /// Execute a `list_memories` query.
    pub async fn list_memories(
        &self,
        params: ListMemoriesParams,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let service = self.require_service()?;
        let limit = params.limit.unwrap_or(20) as usize;
        let project = params.project.as_deref();

        // Treat memory_type=report as persistent filter.
        let persistent_filter = match params.memory_type.as_deref() {
            Some("report") => Some(true),
            Some("memory") => Some(false),
            _ => None,
        };

        match service.list(project, persistent_filter, limit) {
            Ok(memories) => {
                if memories.is_empty() {
                    return Ok(CallToolResult::success(vec![Content::text(
                        "No memories found.",
                    )]));
                }

                let mut text = String::new();
                for m in &memories {
                    let short_id = &m.id[..8.min(m.id.len())];
                    let tags_display = if m.tags.is_empty() {
                        String::new()
                    } else {
                        format!("\n  Tags: {}", m.tags.join(", "))
                    };
                    let project_display = m
                        .project
                        .as_deref()
                        .map(|p| format!("\n  Project: {p}"))
                        .unwrap_or_default();
                    let flags = if m.persistent { " [persistent]" } else { "" };

                    text.push_str(&format!(
                        "- {}{flags} (id: {short_id}){tags_display}{project_display}\n",
                        m.title
                    ));
                }

                Ok(CallToolResult::success(vec![Content::text(text)]))
            }
            Err(e) => Err(rmcp::ErrorData::internal_error(
                format!("Failed to list memories: {e}"),
                None,
            )),
        }
    }
}
