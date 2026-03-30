pub mod concourse;
mod config;
mod error;
mod traits;
mod upstream;

pub use concourse::ConcourseToolSet;
pub use config::*;
pub use error::*;
pub use traits::*;
pub use upstream::*;

use rmcp::model::{CallToolResult, JsonObject, Tool};

pub struct CatalogEntry {
    pub prefixed_name: String,
    pub upstream_name: String,
    pub tool_name: String,
    pub category: Option<String>,
    pub brief_description: String,
    pub full_tool: Tool,
}

pub struct Catalog {
    sets: Vec<Box<dyn ToolSet>>,
}

impl Catalog {
    pub fn instructions(&self) -> String {
        let mut lines = vec![
            "Tools from upstream services are available via progressive disclosure:".to_string(),
            "1. search_tools — discover tools by keyword or category".to_string(),
            "2. describe_tool — get full parameter schema before calling".to_string(),
            "3. call_tool — execute with proper arguments".to_string(),
            String::new(),
            "Available toolsets:".to_string(),
        ];
        for set in &self.sets {
            let cat = set.category().unwrap_or("uncategorized");
            let desc = set.category_description().unwrap_or("");
            let tool_count = set.tools().len();
            lines.push(format!(
                "  {} ({}, {} tools) — {}",
                set.name(),
                cat,
                tool_count,
                desc,
            ));
        }
        lines.join("\n")
    }

    pub fn entries(&self) -> Vec<CatalogEntry> {
        let mut entries = Vec::new();
        for set in &self.sets {
            for tool in set.tools() {
                let desc = &tool.description;
                let brief = desc
                    .description
                    .as_ref()
                    .map(|d| first_sentence(d))
                    .unwrap_or_default();
                entries.push(CatalogEntry {
                    prefixed_name: format!("{}_{}", set.prefix(), desc.name),
                    upstream_name: set.name().to_string(),
                    tool_name: desc.name.to_string(),
                    category: set.category().map(String::from),
                    brief_description: brief,
                    full_tool: desc.clone(),
                });
            }
        }
        entries
    }

    pub fn search(&self, query: Option<&str>, category: Option<&str>) -> Vec<CatalogEntry> {
        self.entries()
            .into_iter()
            .filter(|e| {
                if let Some(cat) = category {
                    if cat != "all" {
                        match &e.category {
                            Some(c) if c == cat => {}
                            _ => return false,
                        }
                    }
                }
                if let Some(q) = query {
                    let q_lower = q.to_lowercase();
                    let matches = e.prefixed_name.to_lowercase().contains(&q_lower)
                        || e.tool_name.to_lowercase().contains(&q_lower)
                        || e.brief_description.to_lowercase().contains(&q_lower)
                        || e.upstream_name.to_lowercase().contains(&q_lower);
                    if !matches {
                        return false;
                    }
                }
                true
            })
            .collect()
    }

    pub fn describe(&self, prefixed_name: &str) -> Option<CatalogEntry> {
        self.entries()
            .into_iter()
            .find(|e| e.prefixed_name == prefixed_name)
    }

    fn find_set<'a>(&'a self, prefixed_name: &'a str) -> Option<(&'a dyn ToolSet, &'a str)> {
        for set in &self.sets {
            let prefix = format!("{}_", set.prefix());
            if let Some(tool_name) = prefixed_name.strip_prefix(&prefix) {
                if set.tools().iter().any(|t| t.name == tool_name) {
                    return Some((set.as_ref(), tool_name));
                }
            }
        }
        None
    }

    pub async fn call(
        &self,
        prefixed_name: &str,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ToolSetsError> {
        let (set, tool_name) = self
            .find_set(prefixed_name)
            .ok_or_else(|| ToolSetsError::ToolNotFound(prefixed_name.to_string()))?;
        set.call(tool_name, arguments).await
    }
}

pub struct ToolSets {
    catalog: Catalog,
}

impl ToolSets {
    #[tracing::instrument(name = "toolset.init", skip_all)]
    pub async fn init(config: ToolSetsConfig) -> Result<Self, ToolSetsError> {
        let mut sets: Vec<Box<dyn ToolSet>> = Vec::new();

        for upstream in &config.mcp_upstreams {
            match UpstreamToolSet::init(upstream).await {
                Ok(ts) => {
                    tracing::info!(
                        name = %upstream.name,
                        prefix = ts.prefix(),
                        tools = ts.tools().len(),
                        "MCP upstream toolset initialized"
                    );
                    sets.push(Box::new(ts));
                }
                Err(e) => {
                    tracing::warn!(name = %upstream.name, error = %e, "Failed to initialize MCP upstream, skipping");
                }
            }
        }

        if config.concourse.enabled
            && !config.concourse.url.is_empty()
            && !config.concourse.username.is_empty()
        {
            let client = concourse_client::ConcourseClient::new(
                &config.concourse.url,
                config.concourse.team.clone(),
                config.concourse.username.clone(),
                config.concourse.password.clone(),
            )?;
            sets.push(Box::new(ConcourseToolSet::new(client)));
            tracing::info!(url = %config.concourse.url, "Concourse toolset initialized");
        }

        Ok(Self {
            catalog: Catalog { sets },
        })
    }

    pub fn add_toolset(&mut self, toolset: Box<dyn ToolSet>) {
        self.catalog.sets.push(toolset);
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }
}

fn first_sentence(s: &str) -> String {
    s.split_once(". ")
        .or_else(|| s.split_once(".\n"))
        .map(|(first, _)| format!("{first}."))
        .unwrap_or_else(|| s.to_string())
}
