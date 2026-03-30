mod config;
mod error;
mod upstream;

pub use config::*;
pub use error::*;
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
    sets: Vec<UpstreamToolSet>,
}

impl Catalog {
    pub fn sets(&self) -> &[UpstreamToolSet] {
        &self.sets
    }

    pub fn entries(&self) -> Vec<CatalogEntry> {
        let mut entries = Vec::new();
        for set in &self.sets {
            for tool in &set.tools {
                let desc = &tool.description;
                let brief = desc
                    .description
                    .as_ref()
                    .map(|d| first_sentence(d))
                    .unwrap_or_default();
                entries.push(CatalogEntry {
                    prefixed_name: format!("{}_{}", set.name, desc.name),
                    upstream_name: set.name.clone(),
                    tool_name: desc.name.to_string(),
                    category: set.category.clone(),
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

    fn find_set_and_tool<'a>(&'a self, prefixed_name: &'a str) -> Option<(&'a UpstreamToolSet, &'a str)> {
        for set in &self.sets {
            let prefix = format!("{}_", set.name);
            if let Some(tool_name) = prefixed_name.strip_prefix(&prefix) {
                if set.tools.iter().any(|t| t.description.name == tool_name) {
                    return Some((set, tool_name));
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
            .find_set_and_tool(prefixed_name)
            .ok_or_else(|| ToolSetsError::ToolNotFound(prefixed_name.to_string()))?;
        set.call_tool(tool_name, arguments).await
    }
}

pub struct ToolSets {
    catalog: Catalog,
}

impl ToolSets {
    #[tracing::instrument(name = "toolset.init", skip_all)]
    pub async fn init(config: ToolSetsConfig) -> Result<Self, ToolSetsError> {
        let mut sets = Vec::new();

        for upstream in &config.mcp_upstreams {
            sets.push(UpstreamToolSet::init(upstream).await?);
        }

        Ok(Self {
            catalog: Catalog { sets },
        })
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
