mod config;

use std::sync::Arc;

use rmcp::model::{CallToolResult, Content, ErrorCode, ErrorData};

use sandbox_client::SandboxClient;

pub use config::SandboxConfig;

/// Holds an initialized sandbox client for use by MCP tool handlers.
#[derive(Clone)]
pub struct SandboxEndpoints {
    client: Arc<SandboxClient>,
}

impl SandboxEndpoints {
    /// Initialize from config. Returns `Ok(None)` if sandbox is disabled.
    pub async fn try_new(config: &SandboxConfig) -> Result<Option<Self>, anyhow::Error> {
        if !config.enabled {
            tracing::info!("Agent Sandbox integration disabled");
            return Ok(None);
        }

        if config.namespace.is_empty() || config.template_name.is_empty() {
            tracing::warn!(
                "Agent Sandbox enabled but namespace/template_name not configured — skipping"
            );
            return Ok(None);
        }

        let client =
            SandboxClient::try_from_env(config.namespace.clone(), config.template_name.clone())
                .await?;

        tracing::info!(
            namespace = %config.namespace,
            template = %config.template_name,
            "Agent Sandbox integration initialized"
        );

        Ok(Some(Self {
            client: Arc::new(client),
        }))
    }

    /// Create a sandbox by issuing a SandboxClaim.
    pub async fn create_sandbox(&self, name: &str) -> Result<CallToolResult, ErrorData> {
        let claim = self.client.create_claim(name).await.map_err(sandbox_err)?;

        let claim_name = claim
            .metadata
            .name
            .unwrap_or_else(|| "<unknown>".to_string());

        let result = serde_json::json!({
            "claim_name": claim_name,
            "status": "created",
            "message": "Sandbox claim created. It will be provisioned from the warm pool.",
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Delete a sandbox by removing its SandboxClaim.
    pub async fn delete_sandbox(&self, name: &str) -> Result<CallToolResult, ErrorData> {
        self.client.delete_claim(name).await.map_err(sandbox_err)?;

        let result = serde_json::json!({
            "claim_name": name,
            "status": "deleted",
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// List all active sandbox claims.
    pub async fn list_sandboxes(&self) -> Result<CallToolResult, ErrorData> {
        let summaries = self.client.list_claims().await.map_err(sandbox_err)?;

        let result: Vec<serde_json::Value> = summaries
            .iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.name,
                    "sandbox_name": s.sandbox_name,
                    "phase": s.phase,
                    "ready": s.ready,
                })
            })
            .collect();

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }

    /// Get status of a single sandbox claim.
    pub async fn get_sandbox(&self, name: &str) -> Result<CallToolResult, ErrorData> {
        let summary = self.client.get_claim(name).await.map_err(sandbox_err)?;

        let result = serde_json::json!({
            "name": summary.name,
            "sandbox_name": summary.sandbox_name,
            "phase": summary.phase,
            "ready": summary.ready,
        });

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        )]))
    }
}

fn sandbox_err(e: sandbox_client::SandboxError) -> ErrorData {
    ErrorData::new(
        ErrorCode::INTERNAL_ERROR,
        format!("Agent Sandbox error: {e}"),
        None::<serde_json::Value>,
    )
}
