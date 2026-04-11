pub mod admin;
mod catalog;
pub mod code_assistant;
pub mod concourse;
mod config;
mod error;
mod filter;
pub mod report;
mod traits;
mod upstream;

pub use admin::AdminToolSet;
pub use catalog::{Catalog, CatalogEntry};
pub use code_assistant::CodeAssistantToolSet;
pub use concourse::ConcourseToolSet;
pub use config::*;
pub use error::*;
pub use filter::OutputFilter;
pub use report::ReportToolSet;
pub use traits::*;
pub use upstream::*;

use std::sync::{Arc, RwLock};

use crate::audit::Audit;
use crate::code_assistant::CodeAssistant;
use crate::report::Reports;

pub struct ToolSets {
    sets: Arc<RwLock<Vec<Arc<dyn ToolSet>>>>,
    audit: Option<Arc<Audit>>,
}

impl ToolSets {
    #[tracing::instrument(name = "toolset.init", skip_all)]
    pub async fn init(
        config: ToolSetsConfig,
        code_assistant: Option<Arc<CodeAssistant>>,
        reports: Option<Arc<Reports>>,
        audit: Option<Arc<Audit>>,
    ) -> Result<Self, ToolSetsError> {
        let mut sets: Vec<Arc<dyn ToolSet>> = Vec::new();

        for upstream in &config.mcp_upstreams {
            match UpstreamToolSet::init(upstream).await {
                Ok(ts) => {
                    tracing::info!(
                        name = %upstream.name,
                        prefix = ts.prefix(),
                        tools = ts.tools().len(),
                        "MCP upstream toolset initialized"
                    );
                    sets.push(Arc::new(ts));
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
            sets.push(Arc::new(ConcourseToolSet::new(client)));
            tracing::info!(url = %config.concourse.url, "Concourse toolset initialized");
        }

        if let Some(ca) = code_assistant {
            sets.push(Arc::new(CodeAssistantToolSet::new(ca)));
            tracing::info!("Code assistant toolset initialized");
        }

        if let Some(rpt) = reports {
            sets.push(Arc::new(ReportToolSet::new(rpt)));
            tracing::info!("Report toolset initialized");
        }

        Ok(Self {
            sets: Arc::new(RwLock::new(sets)),
            audit,
        })
    }

    /// Register a toolset after initialization (e.g. for breaking circular deps).
    pub fn register(&self, toolset: impl ToolSet + 'static) {
        let toolset: Arc<dyn ToolSet> = Arc::new(toolset);
        let mut sets = self.sets.write().expect("toolset lock poisoned");
        tracing::info!(
            name = toolset.name(),
            category = toolset.category(),
            tools = toolset.tools().len(),
            "Late-registered toolset"
        );
        sets.push(toolset);
    }

    /// Build a catalog handle that shares the toolset registry.
    pub fn catalog(&self) -> Catalog {
        Catalog {
            sets: Arc::clone(&self.sets),
            audit: self.audit.clone(),
            auth: None,
            sessions: None,
            session_id: None,
        }
    }
}
