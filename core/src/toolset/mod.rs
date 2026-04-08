mod catalog;
pub mod code_assistant;
pub mod concourse;
mod config;
mod error;
pub mod report;
mod traits;
mod upstream;

pub use catalog::{Catalog, CatalogEntry};
pub use code_assistant::CodeAssistantToolSet;
pub use concourse::ConcourseToolSet;
pub use config::*;
pub use error::*;
pub use report::ReportToolSet;
pub use traits::*;
pub use upstream::*;

use std::sync::Arc;

use crate::audit::Audit;
use crate::code_assistant::CodeAssistant;
use crate::report::Reports;

pub struct ToolSets {
    catalog: Catalog,
}

impl ToolSets {
    #[tracing::instrument(name = "toolset.init", skip_all)]
    pub async fn init(
        config: ToolSetsConfig,
        code_assistant: Option<Arc<CodeAssistant>>,
        reports: Option<Arc<Reports>>,
        audit: Option<Arc<Audit>>,
    ) -> Result<Self, ToolSetsError> {
        let mut sets: Vec<Box<dyn ToolSet>> = Vec::new();

        for upstream in &config.mcp_upstreams {
            if upstream.auth_header.is_empty() {
                tracing::warn!(name = %upstream.name, "Skipping upstream — no auth header set");
                continue;
            }
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

        if let Some(ca) = code_assistant {
            sets.push(Box::new(CodeAssistantToolSet::new(ca)));
            tracing::info!("Code assistant toolset initialized");
        }

        if let Some(rpt) = reports {
            sets.push(Box::new(ReportToolSet::new(rpt)));
            tracing::info!("Report toolset initialized");
        }

        Ok(Self {
            catalog: Catalog {
                sets: Arc::new(sets),
                audit,
                auth: None,
            },
        })
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }
}
